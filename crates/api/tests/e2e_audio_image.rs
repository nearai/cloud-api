//! E2E tests for audio input/output and image generation features.
//!
//! These tests can run in two modes:
//! - With mocks (default, for CI pipeline): `cargo test --test e2e_audio_image`
//! - With real providers (for dev testing): `USE_REAL_PROVIDERS=true cargo test --test e2e_audio_image`

mod common;

use common::*;

/// Test audio input in chat completions (sending audio data to the model)
#[tokio::test]
async fn test_audio_input_chat_completion() {
    let use_real = use_real_providers();
    println!(
        "Running audio input test with {} providers",
        if use_real { "REAL" } else { "MOCK" }
    );

    let (server, api_key, _guard) = if use_real {
        let (server, _pool, _db, guard) = setup_test_server_real_providers().await;
        setup_qwen_omni_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await; // $10.00 USD
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    } else {
        let (server, guard) = setup_test_server().await;
        setup_qwen_omni_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    };

    // Create test audio data
    let audio_base64 = create_test_audio_base64();

    // Send chat completion request with audio input
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Omni-30B-A3B-Instruct",
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "input_audio",
                        "input_audio": {
                            "data": audio_base64,
                            "format": "wav"
                        }
                    },
                    {
                        "type": "text",
                        "text": "What do you hear in this audio?"
                    }
                ]
            }],
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    println!("Audio input response status: {}", response.status_code());

    // With mocks, we expect success (mocks accept any input)
    // With real providers, we expect success if the model supports audio
    if !use_real {
        assert_eq!(
            response.status_code(),
            200,
            "Audio input should be accepted"
        );

        let response_text = response.text();
        println!("Response: {}", response_text);

        // Verify we got a streaming response
        assert!(
            response_text.contains("data:"),
            "Expected SSE streaming response"
        );
    } else {
        // Real providers should process audio input and return a response
        let status = response.status_code();
        assert_eq!(status, 200, "Audio input request should succeed");

        let response_text = response.text();
        println!("Response length: {} bytes", response_text.len());

        // Verify we got a streaming response
        assert!(
            response_text.contains("data:"),
            "Expected SSE streaming response"
        );

        // Parse the stream and verify we got a meaningful response
        let mut text_content = String::new();
        let mut found_response = false;

        for line in response_text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data.trim() == "[DONE]" {
                    break;
                }

                // Parse JSON to extract content
                if let Ok(chunk_json) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(choices) = chunk_json.get("choices").and_then(|c| c.as_array()) {
                        for choice in choices {
                            if let Some(delta) = choice.get("delta") {
                                if let Some(content) = delta.get("content").and_then(|c| c.as_str())
                                {
                                    text_content.push_str(content);
                                    found_response = true;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Verify we got a response (the model should have processed the audio)
        assert!(
            found_response,
            "Expected to receive a response after processing audio input"
        );
        assert!(
            !text_content.trim().is_empty(),
            "Expected non-empty response content after processing audio, got: '{}'",
            text_content
        );

        println!("Received response after audio input: '{}'", text_content);
    }

    // Allow background tasks (usage recording) to complete before test cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

/// Test audio output from chat completions (requesting audio response)
/// Note: vLLM requires ["text", "audio"] modalities (not just ["audio"])
#[tokio::test]
async fn test_audio_output_chat_completion() {
    let use_real = use_real_providers();
    println!(
        "Running audio output test with {} providers",
        if use_real { "REAL" } else { "MOCK" }
    );

    let (server, api_key, _guard) = if use_real {
        let (server, _pool, _db, guard) = setup_test_server_real_providers().await;
        setup_qwen_omni_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    } else {
        let (server, guard) = setup_test_server().await;
        setup_qwen_omni_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    };

    // Send chat completion request with modalities: ["text", "audio"] for audio output
    // Note: vLLM has a bug with ["audio"] alone, so we use ["text", "audio"]
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Omni-30B-A3B-Instruct",
            "messages": [{
                "role": "user",
                "content": "Say hello in a friendly way"
            }],
            "modalities": ["text", "audio"],
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    println!("Audio output response status: {}", response.status_code());

    if !use_real {
        // With mocks, we just verify the request is accepted
        // The mock doesn't actually generate audio output
        assert_eq!(
            response.status_code(),
            200,
            "Audio output request should be accepted"
        );

        let response_text = response.text();
        println!("Response: {}", response_text);

        // Verify we got a streaming response
        assert!(
            response_text.contains("data:"),
            "Expected SSE streaming response"
        );
    } else {
        // Real providers should return audio in the response
        let status = response.status_code();
        assert_eq!(status, 200, "Audio output request should succeed");

        let response_text = response.text();
        println!("Response length: {} bytes", response_text.len());
        println!(
            "First 500 chars of response: {}",
            &response_text.chars().take(500).collect::<String>()
        );

        // Verify we got a streaming response
        assert!(
            response_text.contains("data:"),
            "Expected SSE streaming response, got: {}",
            response_text
        );

        // Parse the stream and verify audio modality
        let mut found_audio_modality = false;
        let mut found_text_modality = false;
        let mut audio_content_length = 0;
        let mut chunk_count = 0;

        for line in response_text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data.trim() == "[DONE]" {
                    break;
                }

                chunk_count += 1;

                // Parse JSON to check modality
                if let Ok(chunk_json) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(modality) = chunk_json.get("modality").and_then(|m| m.as_str()) {
                        match modality {
                            "audio" => {
                                found_audio_modality = true;
                                // Check if this chunk has audio content
                                if let Some(choices) =
                                    chunk_json.get("choices").and_then(|c| c.as_array())
                                {
                                    for choice in choices {
                                        if let Some(delta) = choice.get("delta") {
                                            if let Some(content) =
                                                delta.get("content").and_then(|c| c.as_str())
                                            {
                                                audio_content_length += content.len();
                                            }
                                        }
                                    }
                                }
                            }
                            "text" => {
                                found_text_modality = true;
                            }
                            _ => {}
                        }
                    } else {
                        // Log chunks without modality for debugging
                        if chunk_count <= 5 {
                            println!(
                                "Chunk {} has no modality field: {}",
                                chunk_count,
                                data.chars().take(200).collect::<String>()
                            );
                        }
                    }
                } else {
                    // Log parse errors for debugging
                    if chunk_count <= 5 {
                        println!(
                            "Failed to parse chunk {}: {}",
                            chunk_count,
                            data.chars().take(200).collect::<String>()
                        );
                    }
                }
            }
        }

        println!(
            "Parsed {} chunks, found audio modality: {}, text modality: {}, audio content: {} bytes",
            chunk_count, found_audio_modality, found_text_modality, audio_content_length
        );

        // Verify we got audio modality chunks
        assert!(
            found_audio_modality,
            "Expected to find chunks with 'modality': 'audio' in the response. Parsed {} chunks. Response: {}",
            chunk_count,
            response_text.chars().take(1000).collect::<String>()
        );

        // Verify audio content is present (base64 encoded audio data)
        assert!(
            audio_content_length > 0,
            "Expected audio content in chunks with 'modality': 'audio', got {} bytes",
            audio_content_length
        );
    }

    // Allow background tasks (usage recording) to complete before test cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

/// Test audio output with text+audio modalities
#[tokio::test]
async fn test_audio_and_text_output_chat_completion() {
    let use_real = use_real_providers();
    println!(
        "Running audio+text output test with {} providers",
        if use_real { "REAL" } else { "MOCK" }
    );

    let (server, api_key, _guard) = if use_real {
        let (server, _pool, _db, guard) = setup_test_server_real_providers().await;
        setup_qwen_omni_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    } else {
        let (server, guard) = setup_test_server().await;
        setup_qwen_omni_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    };

    // Send chat completion request with modalities: ["text", "audio"]
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Omni-30B-A3B-Instruct",
            "messages": [{
                "role": "user",
                "content": "Say hello in both text and audio"
            }],
            "modalities": ["text", "audio"],
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    println!(
        "Audio+text output response status: {}",
        response.status_code()
    );

    if !use_real {
        assert_eq!(
            response.status_code(),
            200,
            "Audio+text output request should be accepted"
        );

        let response_text = response.text();
        println!("Response: {}", response_text);

        assert!(
            response_text.contains("data:"),
            "Expected SSE streaming response"
        );
    } else {
        let status = response.status_code();
        assert_eq!(status, 200, "Audio+text output request should succeed");

        let response_text = response.text();
        println!("Response length: {} bytes", response_text.len());

        // Verify we got a streaming response
        assert!(
            response_text.contains("data:"),
            "Expected SSE streaming response"
        );

        // Parse the stream and verify both modalities
        let mut found_audio_modality = false;
        let mut found_text_modality = false;
        let mut text_content = String::new();
        let mut audio_content_length = 0;

        for line in response_text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data.trim() == "[DONE]" {
                    break;
                }

                // Parse JSON to check modality
                if let Ok(chunk_json) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(modality) = chunk_json.get("modality").and_then(|m| m.as_str()) {
                        if let Some(choices) = chunk_json.get("choices").and_then(|c| c.as_array())
                        {
                            for choice in choices {
                                if let Some(delta) = choice.get("delta") {
                                    if let Some(content) =
                                        delta.get("content").and_then(|c| c.as_str())
                                    {
                                        match modality {
                                            "audio" => {
                                                found_audio_modality = true;
                                                audio_content_length += content.len();
                                            }
                                            "text" => {
                                                found_text_modality = true;
                                                text_content.push_str(content);
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Verify we got both modalities
        assert!(
            found_text_modality,
            "Expected to find chunks with 'modality': 'text' in the response"
        );
        assert!(
            found_audio_modality,
            "Expected to find chunks with 'modality': 'audio' in the response"
        );

        // Verify we got actual content
        assert!(
            !text_content.is_empty(),
            "Expected non-empty text content, got: '{}'",
            text_content
        );
        assert!(
            audio_content_length > 0,
            "Expected audio content in chunks with 'modality': 'audio', got {} bytes",
            audio_content_length
        );

        println!(
            "Found text modality: {}, audio modality: {}, text: '{}', audio: {} bytes",
            found_text_modality, found_audio_modality, text_content, audio_content_length
        );
    }

    // Allow background tasks (usage recording) to complete before test cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

/// Test image generation endpoint
#[tokio::test]
async fn test_image_generation() {
    let use_real = use_real_providers();
    println!(
        "Running image generation test with {} providers",
        if use_real { "REAL" } else { "MOCK" }
    );

    let (server, api_key, _guard) = if use_real {
        let (server, _pool, _db, guard) = setup_test_server_real_providers().await;
        setup_qwen_image_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    } else {
        let (server, guard) = setup_test_server().await;
        setup_qwen_image_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    };

    // Send image generation request
    let response = server
        .post("/v1/images/generations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen-Image-2512",
            "prompt": "A beautiful sunset over mountains with orange and purple sky",
            "n": 1,
            "response_format": "b64_json"
        }))
        .await;

    println!(
        "Image generation response status: {}",
        response.status_code()
    );

    if !use_real {
        // With mocks, verify the response structure
        assert_eq!(
            response.status_code(),
            200,
            "Image generation should succeed"
        );

        let response_json: serde_json::Value = response.json();
        println!(
            "Response: {}",
            serde_json::to_string_pretty(&response_json).unwrap()
        );

        // Verify response structure
        assert!(
            response_json.get("created").is_some(),
            "Should have created timestamp"
        );
        assert!(
            response_json.get("data").is_some(),
            "Should have data array"
        );

        let data = response_json.get("data").unwrap().as_array().unwrap();
        assert!(!data.is_empty(), "Should have at least one image");

        let first_image = &data[0];
        assert!(
            first_image.get("b64_json").is_some(),
            "Should have b64_json field"
        );

        let b64_data = first_image.get("b64_json").unwrap().as_str().unwrap();
        assert!(!b64_data.is_empty(), "b64_json should not be empty");
        println!("Image data length: {} chars", b64_data.len());
    } else {
        let status = response.status_code();
        println!("Real provider response status: {}", status);

        if status == 200 {
            let response_json: serde_json::Value = response.json();
            println!(
                "Response: {}",
                serde_json::to_string_pretty(&response_json).unwrap()
            );

            // Verify response structure for real provider
            assert!(
                response_json.get("data").is_some(),
                "Should have data array"
            );

            let data = response_json.get("data").unwrap().as_array().unwrap();
            if !data.is_empty() {
                let first_image = &data[0];
                if let Some(b64_data) = first_image.get("b64_json") {
                    let b64_str = b64_data.as_str().unwrap_or("");
                    println!("Real image data length: {} chars", b64_str.len());

                    // Verify it's valid base64
                    if !b64_str.is_empty() {
                        use base64::Engine;
                        let decode_result =
                            base64::engine::general_purpose::STANDARD.decode(b64_str);
                        assert!(decode_result.is_ok(), "Image data should be valid base64");
                        println!("Decoded image size: {} bytes", decode_result.unwrap().len());
                    }
                }
            }
        }
    }

    // Allow background tasks (usage recording) to complete before test cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

/// Test image generation with multiple images
#[tokio::test]
async fn test_image_generation_multiple() {
    let use_real = use_real_providers();
    println!(
        "Running multiple image generation test with {} providers",
        if use_real { "REAL" } else { "MOCK" }
    );

    let (server, api_key, _guard) = if use_real {
        let (server, _pool, _db, guard) = setup_test_server_real_providers().await;
        setup_qwen_image_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    } else {
        let (server, guard) = setup_test_server().await;
        setup_qwen_image_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    };

    // Request multiple images
    let response = server
        .post("/v1/images/generations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen-Image-2512",
            "prompt": "A cute cat sitting on a windowsill",
            "n": 2,
            "response_format": "b64_json"
        }))
        .await;

    println!(
        "Multiple image generation response status: {}",
        response.status_code()
    );

    if !use_real {
        assert_eq!(
            response.status_code(),
            200,
            "Multiple image generation should succeed"
        );

        let response_json: serde_json::Value = response.json();

        let data = response_json.get("data").unwrap().as_array().unwrap();
        assert_eq!(data.len(), 2, "Should have 2 images as requested");

        for (i, image) in data.iter().enumerate() {
            assert!(
                image.get("b64_json").is_some(),
                "Image {} should have b64_json field",
                i
            );
        }
    } else {
        let status = response.status_code();
        if status == 200 {
            let response_json: serde_json::Value = response.json();
            let data = response_json.get("data").unwrap().as_array().unwrap();
            println!("Real provider returned {} images", data.len());
        }
    }

    // Allow background tasks (usage recording) to complete before test cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

/// Test image generation validation errors
#[tokio::test]
async fn test_image_generation_validation() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_qwen_image_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Test with invalid n value (too high)
    let response = server
        .post("/v1/images/generations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen-Image-2512",
            "prompt": "A beautiful sunset",
            "n": 100,  // Too many images
            "response_format": "b64_json"
        }))
        .await;

    // Should fail validation
    assert_eq!(response.status_code(), 400, "Should reject n > 10");

    // Test with empty prompt
    let response = server
        .post("/v1/images/generations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen-Image-2512",
            "prompt": "",
            "response_format": "b64_json"
        }))
        .await;

    // Should fail validation
    assert_eq!(response.status_code(), 400, "Should reject empty prompt");
}

/// Test response_format validation for verifiable models (attestation_supported = true)
/// Verifiable models only support "b64_json" format:
/// - response_format: "url" should be rejected
/// - response_format omitted should default to "b64_json"
/// - response_format: "b64_json" should work
/// - Any other value should be rejected
#[tokio::test]
async fn test_verifiable_model_response_format_validation() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    // Set up the verifiable image model (Qwen/Qwen-Image-2512 has verifiable=true and defaults to attestation_supported=true)
    setup_qwen_image_model(&server).await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Test 1: response_format: "url" should be rejected
    let response = server
        .post("/v1/images/generations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen-Image-2512",
            "prompt": "A beautiful sunset",
            "response_format": "url"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should reject response_format 'url' for verifiable models"
    );
    let error_json: serde_json::Value = response.json();
    let error_message = error_json
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    assert!(
        error_message.contains("not supported for verifiable models"),
        "Error message should mention verifiable models, got: {}",
        error_message
    );
    assert!(
        error_message.contains("b64_json"),
        "Error message should mention b64_json, got: {}",
        error_message
    );

    // Test 2: response_format omitted should default to "b64_json" and succeed
    let response = server
        .post("/v1/images/generations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen-Image-2512",
            "prompt": "A beautiful sunset"
            // response_format omitted
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Should accept omitted response_format and default to b64_json"
    );
    let response_json: serde_json::Value = response.json();
    let data = response_json
        .get("data")
        .and_then(|d| d.as_array())
        .expect("Should have data array");
    assert!(!data.is_empty(), "Should have at least one image");
    let first_image = &data[0];
    assert!(
        first_image.get("b64_json").is_some(),
        "Should have b64_json field when response_format is omitted (defaulted to b64_json)"
    );

    // Test 3: response_format: "b64_json" should work
    let response = server
        .post("/v1/images/generations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen-Image-2512",
            "prompt": "A beautiful sunset",
            "response_format": "b64_json"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Should accept response_format 'b64_json' for verifiable models"
    );
    let response_json: serde_json::Value = response.json();
    let data = response_json
        .get("data")
        .and_then(|d| d.as_array())
        .expect("Should have data array");
    assert!(!data.is_empty(), "Should have at least one image");
    let first_image = &data[0];
    assert!(
        first_image.get("b64_json").is_some(),
        "Should have b64_json field"
    );

    // Test 4: Any other value should be rejected
    let response = server
        .post("/v1/images/generations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen-Image-2512",
            "prompt": "A beautiful sunset",
            "response_format": "invalid_format"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should reject invalid response_format values for verifiable models"
    );
    let error_json: serde_json::Value = response.json();
    let error_message = error_json
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    assert!(
        error_message.contains("response_format must be 'url' or 'b64_json'"),
        "Error message should mention valid response formats, got: {}",
        error_message
    );
}

/// Test that audio content is properly passed through to providers
#[tokio::test]
async fn test_audio_content_passthrough() {
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_qwen_omni_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let audio_base64 = create_test_audio_base64();

    // Test with both audio and text content parts
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-Omni-30B-A3B-Instruct",
            "messages": [
                {
                    "role": "system",
                    "content": "You are a helpful audio assistant."
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": "Please analyze this audio:"
                        },
                        {
                            "type": "input_audio",
                            "input_audio": {
                                "data": audio_base64,
                                "format": "wav"
                            }
                        }
                    ]
                }
            ],
            "stream": false,
            "max_tokens": 50
        }))
        .await;

    println!(
        "Audio passthrough response status: {}",
        response.status_code()
    );
    assert_eq!(
        response.status_code(),
        200,
        "Mixed text+audio content should be accepted"
    );

    let response_json: serde_json::Value = response.json();
    println!(
        "Response: {}",
        serde_json::to_string_pretty(&response_json).unwrap()
    );

    // Verify response structure
    assert!(
        response_json.get("choices").is_some(),
        "Should have choices"
    );
    let choices = response_json.get("choices").unwrap().as_array().unwrap();
    assert!(!choices.is_empty(), "Should have at least one choice");

    // Allow background tasks (usage recording) to complete before test cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

/// Test that has_audio_content correctly identifies audio in requests
#[tokio::test]
async fn test_has_audio_content_detection() {
    // This tests the API layer's audio content detection
    let (server, guard) = setup_test_server().await;
    let _guard = guard;

    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Request without audio should work
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{
                "role": "user",
                "content": "Hello, how are you?"
            }],
            "stream": false,
            "max_tokens": 50
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Text-only request should succeed"
    );

    // Request with audio in content parts
    let audio_base64 = create_test_audio_base64();
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "input_audio",
                    "input_audio": {
                        "data": audio_base64,
                        "format": "wav"
                    }
                }]
            }],
            "stream": false,
            "max_tokens": 50
        }))
        .await;

    // Audio content is now allowed for passthrough
    assert_eq!(
        response.status_code(),
        200,
        "Audio content should be accepted for passthrough"
    );

    // Allow background tasks (usage recording) to complete before test cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

// ============================================
// Attestation Tests for Audio Output
// ============================================

/// Mock ECDSA public key for testing
const MOCK_ECDSA_PUB_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Helper function to fetch the model public key from the attestation report endpoint
async fn get_model_public_key(
    server: &axum_test::TestServer,
    model: &str,
    signing_algo: Option<&str>,
) -> Option<String> {
    let encoded_model = url::form_urlencoded::byte_serialize(model.as_bytes()).collect::<String>();
    let mut url = format!("/v1/attestation/report?model={}", encoded_model);
    if let Some(algo) = signing_algo {
        let encoded_algo =
            url::form_urlencoded::byte_serialize(algo.as_bytes()).collect::<String>();
        url.push_str(&format!("&signing_algo={}", encoded_algo));
    }

    let response = server.get(&url).await;

    if response.status_code() != 200 {
        return None;
    }

    let response_json: serde_json::Value = response.json();

    // Try to get signing_public_key from model_attestations
    if let Some(model_attestations) = response_json
        .get("model_attestations")
        .and_then(|v| v.as_array())
    {
        for attestation in model_attestations {
            if let Some(signing_public_key) = attestation.get("signing_public_key") {
                if let Some(key_str) = signing_public_key.as_str() {
                    return Some(key_str.to_string());
                }
            }
        }
    }

    None
}

/// Test that streaming audio output with encryption headers works correctly
/// Note: This test only uses mock providers because real encryption requires valid client keys
#[tokio::test]
async fn test_audio_output_with_encryption_headers() {
    // This test only runs with mock providers since real encryption requires valid keypairs
    // Real provider attestation is tested in test_audio_output_attestation_report
    if use_real_providers() {
        println!(
            "Skipping encryption headers test with real providers (requires valid client keys)"
        );
        return;
    }

    println!("Running audio output attestation test with MOCK providers");

    let (server, guard) = setup_test_server().await;
    setup_qwen_omni_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let _guard = guard;

    let model = "Qwen/Qwen3-Omni-30B-A3B-Instruct";

    // Get the model public key from attestation endpoint
    let model_pub_key = get_model_public_key(&server, model, Some("ecdsa"))
        .await
        .unwrap_or_else(|| MOCK_ECDSA_PUB_KEY.to_string());

    // Send chat completion request with audio output and encryption headers
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .add_header("X-Signing-Algo", "ecdsa")
        .add_header("X-Client-Pub-Key", MOCK_ECDSA_PUB_KEY)
        .add_header("X-Model-Pub-Key", model_pub_key)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{
                "role": "user",
                "content": "Say hello"
            }],
            "modalities": ["text", "audio"],
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    println!(
        "Audio output with encryption response status: {}",
        response.status_code()
    );

    // Request should succeed
    assert_eq!(
        response.status_code(),
        200,
        "Audio output request with encryption headers should succeed"
    );

    let response_text = response.text();

    // Verify we got a streaming response
    assert!(
        response_text.contains("data:"),
        "Expected SSE streaming response"
    );

    // Check for Inference-Id header (used for signature retrieval)
    let inference_id = response.headers().get("inference-id");
    println!("Inference-Id header: {:?}", inference_id);

    // Allow background tasks to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

/// Test that attestation report endpoint works for audio model
#[tokio::test]
async fn test_audio_output_attestation_report() {
    let use_real = use_real_providers();
    println!(
        "Running audio attestation report test with {} providers",
        if use_real { "REAL" } else { "MOCK" }
    );

    let (server, api_key, _guard) = if use_real {
        let (server, _pool, _db, guard) = setup_test_server_real_providers().await;
        setup_qwen_omni_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    } else {
        let (server, guard) = setup_test_server().await;
        setup_qwen_omni_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    };

    let model = "Qwen/Qwen3-Omni-30B-A3B-Instruct";

    // Test attestation report endpoint for the audio model
    let encoded_model = url::form_urlencoded::byte_serialize(model.as_bytes()).collect::<String>();
    let url = format!("/v1/attestation/report?model={}", encoded_model);

    let response = server.get(&url).await;
    println!(
        "Attestation report response status: {}",
        response.status_code()
    );

    // Attestation report should be available
    assert_eq!(
        response.status_code(),
        200,
        "Attestation report should be available for audio model"
    );

    let response_json: serde_json::Value = response.json();
    println!(
        "Attestation report: {}",
        serde_json::to_string_pretty(&response_json).unwrap_or_default()
    );

    // Verify the response has expected structure
    assert!(
        response_json.get("model_attestations").is_some(),
        "Attestation report should contain model_attestations"
    );

    // Now make an audio output request without encryption (just to verify it works)
    let completion_response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{
                "role": "user",
                "content": "Say hello"
            }],
            "modalities": ["text", "audio"],
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    assert_eq!(
        completion_response.status_code(),
        200,
        "Audio output request should succeed"
    );

    let response_text = completion_response.text();
    assert!(
        response_text.contains("data:"),
        "Expected SSE streaming response"
    );

    if use_real {
        // Verify we got modality chunks
        let mut found_modality = false;
        for line in response_text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data.trim() == "[DONE]" {
                    break;
                }
                if let Ok(chunk_json) = serde_json::from_str::<serde_json::Value>(data) {
                    if chunk_json.get("modality").is_some() {
                        found_modality = true;
                        break;
                    }
                }
            }
        }
        assert!(
            found_modality,
            "Expected to find modality field in response chunks"
        );
    }

    // Allow background tasks to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}

/// Test that signature endpoint works for audio completions with full hash verification
/// This verifies:
/// 1. The request hash matches what we sent
/// 2. The response hash matches the accumulated SSE response
/// 3. The cryptographic signature is valid (with real providers)
#[tokio::test]
async fn test_audio_output_signature_verification() {
    let use_real = use_real_providers();
    println!(
        "Running audio output signature verification test with {} providers",
        if use_real { "REAL" } else { "MOCK" }
    );

    let (server, api_key, _guard) = if use_real {
        let (server, _pool, _db, guard) = setup_test_server_real_providers().await;
        setup_qwen_omni_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    } else {
        let (server, guard) = setup_test_server().await;
        setup_qwen_omni_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        (server, api_key, guard)
    };

    let model = "Qwen/Qwen3-Omni-30B-A3B-Instruct";

    // Step 1: Construct the request body
    let request_body = serde_json::json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": "Say hi"
        }],
        "modalities": ["text", "audio"],
        "stream": true,
        "max_tokens": 50
    });

    // Step 2: Compute expected request hash
    let request_json =
        serde_json::to_string(&request_body).expect("Failed to serialize request body");
    let expected_request_hash = compute_sha256(&request_json);
    println!("Expected request hash: {}", expected_request_hash);

    // Step 3: Make the audio output request
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request_body)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Audio output request should succeed"
    );

    // Step 4: Capture complete raw response and compute its hash
    let response_text = response.text();
    let expected_response_hash = compute_sha256(&response_text);
    println!("Expected response hash: {}", expected_response_hash);
    println!(
        "Response length: {} bytes, first 200 chars: {}",
        response_text.len(),
        response_text.chars().take(200).collect::<String>()
    );

    // Step 5: Extract chat_id from the response
    let mut chat_id: Option<String> = None;
    let mut found_audio_modality = false;
    let mut found_text_modality = false;

    for line in response_text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data.trim() == "[DONE]" {
                break;
            }
            if let Ok(chunk_json) = serde_json::from_str::<serde_json::Value>(data) {
                if chat_id.is_none() {
                    if let Some(id) = chunk_json.get("id").and_then(|v| v.as_str()) {
                        chat_id = Some(id.to_string());
                    }
                }
                // Track modalities for verification
                if let Some(modality) = chunk_json.get("modality").and_then(|m| m.as_str()) {
                    match modality {
                        "audio" => found_audio_modality = true,
                        "text" => found_text_modality = true,
                        _ => {}
                    }
                }
            }
        }
    }

    println!("Extracted chat_id: {:?}", chat_id);
    println!(
        "Found modalities - text: {}, audio: {}",
        found_text_modality, found_audio_modality
    );
    assert!(
        chat_id.is_some(),
        "Should have extracted chat_id from response"
    );

    // Allow time for signature to be stored
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let id = chat_id.unwrap();
    let encoded_id = url::form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>();

    // Step 6: Test ECDSA signature with full hash verification
    {
        let sig_url = format!(
            "/v1/signature/{}?model={}&signing_algo=ecdsa",
            encoded_id, model
        );

        let sig_response = server
            .get(&sig_url)
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await;

        assert_eq!(
            sig_response.status_code(),
            200,
            "ECDSA signature endpoint should return 200"
        );

        let sig_json: serde_json::Value = sig_response.json();
        println!(
            "ECDSA Signature response: {}",
            serde_json::to_string_pretty(&sig_json).unwrap_or_default()
        );

        // Parse signature fields
        let text = sig_json
            .get("text")
            .and_then(|v| v.as_str())
            .expect("Signature should have 'text' field");
        let signature = sig_json
            .get("signature")
            .and_then(|v| v.as_str())
            .expect("Signature should have 'signature' field");
        let signing_address = sig_json
            .get("signing_address")
            .and_then(|v| v.as_str())
            .expect("Signature should have 'signing_address' field");
        let signing_algo = sig_json
            .get("signing_algo")
            .and_then(|v| v.as_str())
            .expect("Signature should have 'signing_algo' field");

        assert_eq!(signing_algo, "ecdsa", "Signing algorithm should be ecdsa");

        // Parse signature text (format: "request_hash:response_hash")
        let hash_parts: Vec<&str> = text.split(':').collect();
        assert_eq!(
            hash_parts.len(),
            2,
            "Signature text should contain two hashes separated by ':'"
        );

        let actual_request_hash = hash_parts[0];
        let actual_response_hash = hash_parts[1];

        println!("\n=== ECDSA Hash Verification ===");
        println!("Request hash  - Expected: {}", expected_request_hash);
        println!("Request hash  - Actual:   {}", actual_request_hash);
        println!("Response hash - Expected: {}", expected_response_hash);
        println!("Response hash - Actual:   {}", actual_response_hash);

        // Verify request hash matches
        assert_eq!(
            expected_request_hash, actual_request_hash,
            "\n❌ REQUEST HASH MISMATCH for audio output!\n\
             Expected: {}\n\
             Actual:   {}\n",
            expected_request_hash, actual_request_hash
        );
        println!("✅ Request hash matches!");

        // Verify response hash matches
        assert_eq!(
            expected_response_hash, actual_response_hash,
            "\n❌ RESPONSE HASH MISMATCH for audio output!\n\
             Expected: {}\n\
             Actual:   {}\n\
             This means the signature doesn't match the audio response we received.\n",
            expected_response_hash, actual_response_hash
        );
        println!("✅ Response hash matches - audio output integrity verified!");

        // Cryptographically verify only with real providers
        if use_real {
            let is_valid = verify_ecdsa_signature(text, signature, signing_address);
            assert!(
                is_valid,
                "ECDSA signature should be cryptographically valid for audio output"
            );
            println!("✅ ECDSA signature cryptographically verified!");
        }
    }

    // Step 7: Test Ed25519 signature with full hash verification
    {
        let sig_url = format!(
            "/v1/signature/{}?model={}&signing_algo=ed25519",
            encoded_id, model
        );

        let sig_response = server
            .get(&sig_url)
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await;

        assert_eq!(
            sig_response.status_code(),
            200,
            "Ed25519 signature endpoint should return 200"
        );

        let sig_json: serde_json::Value = sig_response.json();

        // Parse signature fields
        let text = sig_json
            .get("text")
            .and_then(|v| v.as_str())
            .expect("Signature should have 'text' field");
        let signature = sig_json
            .get("signature")
            .and_then(|v| v.as_str())
            .expect("Signature should have 'signature' field");
        let signing_address = sig_json
            .get("signing_address")
            .and_then(|v| v.as_str())
            .expect("Signature should have 'signing_address' field");
        let signing_algo = sig_json
            .get("signing_algo")
            .and_then(|v| v.as_str())
            .expect("Signature should have 'signing_algo' field");

        assert_eq!(
            signing_algo, "ed25519",
            "Signing algorithm should be ed25519"
        );

        // Parse and verify hashes (same as ECDSA)
        let hash_parts: Vec<&str> = text.split(':').collect();
        assert_eq!(hash_parts.len(), 2);

        let actual_request_hash = hash_parts[0];
        let actual_response_hash = hash_parts[1];

        println!("\n=== Ed25519 Hash Verification ===");
        assert_eq!(
            expected_request_hash, actual_request_hash,
            "Ed25519 request hash should match"
        );
        assert_eq!(
            expected_response_hash, actual_response_hash,
            "Ed25519 response hash should match"
        );
        println!("✅ Ed25519 hashes verified!");

        // Cryptographically verify only with real providers
        if use_real {
            let is_valid = verify_ed25519_signature(text, signature, signing_address);
            assert!(
                is_valid,
                "Ed25519 signature should be cryptographically valid for audio output"
            );
            println!("✅ Ed25519 signature cryptographically verified!");
        }
    }

    println!("\n=== Audio Output Attestation Summary ===");
    println!("✅ Request hash verified (client can prove what they asked)");
    println!("✅ Response hash verified (client can prove what they received)");
    println!("✅ Signatures retrieved for both ECDSA and Ed25519");
    if use_real {
        println!("✅ Cryptographic signatures verified");
    }
    println!("✅ Audio output can be fully attested by TEE clients!");

    // Allow background tasks to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
}
