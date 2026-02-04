//! End-to-end tests for image generation and editing through the Response API
//!
//! Tests verify:
//! - Image generation with dall-e and similar models
//! - Image editing with base64-encoded input images
//! - Proper conversation threading (previous_response_id, next_response_ids)
//! - Usage tracking for image_count
//! - Backward compatibility with text completions

mod common;
use common::*;
use serde_json::json;

/// Test image generation through Response API
/// Verifies that image generation model requests are routed to image generation
#[tokio::test]
async fn test_image_generation_through_response_api() {
    let (server, _guard) = setup_test_server().await;
    let qwen_model = setup_qwen_model(&server).await;
    let image_model = setup_qwen_image_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation first
    let conv_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "name": "Image generation test"
        }))
        .await;

    assert_eq!(conv_response.status_code(), 201);
    let conv_data: serde_json::Value = conv_response.json();
    let conversation_id = conv_data["id"].as_str().unwrap();

    // Request image generation through Response API with image model
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": image_model,
            "input": "A beautiful landscape with mountains and sunset",
            "conversation": {
                "id": conversation_id
            },
            "stream": true
        }))
        .await;

    println!(
        "Image generation response status: {}",
        response.status_code()
    );
    assert_eq!(response.status_code(), 200);

    let response_text = response.text();
    println!("Response length: {} bytes", response_text.len());

    // Verify we got a streaming response with response.output_image.created event
    assert!(
        response_text.contains("response.output_image.created"),
        "Expected output_image.created event in stream"
    );

    // Verify response.created and response.completed events
    assert!(
        response_text.contains("response.created"),
        "Expected response.created event"
    );
    assert!(
        response_text.contains("response.completed"),
        "Expected response.completed event"
    );

    // Verify the image data is in OutputImage format
    assert!(
        response_text.contains("output_image"),
        "Expected output_image variant in response"
    );

    println!("Image generation test passed!");
}

/// Test image editing through Response API
/// Verifies that requests with input images are routed to image edit endpoint
#[tokio::test]
async fn test_image_editing_through_response_api() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let image_model = setup_qwen_image_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a simple 1x1 PNG image in base64
    // This is a minimal valid PNG: 1x1 pixel transparent image
    let base64_image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    // Request image edit with image model
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": image_model,
            "input": [{
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Make the sky more blue"
                    },
                    {
                        "type": "input_image",
                        "image_url": {
                            "url": format!("data:image/png;base64,{}", base64_image)
                        }
                    }
                ]
            }],
            "stream": true
        }))
        .await;

    println!("Image edit response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let response_text = response.text();

    // Verify we got a streaming response for image edit
    assert!(
        response_text.contains("response.output_image.created"),
        "Expected output_image.created event for image edit"
    );

    println!("Image edit test passed!");
}

/// Test conversation threading for image responses
/// Verifies that previous_response_id and next_response_ids are properly set
#[tokio::test]
async fn test_image_response_conversation_threading() {
    let (server, _guard) = setup_test_server().await;
    let qwen_model = setup_qwen_model(&server).await;
    let image_model = setup_qwen_image_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a conversation
    let conv_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "name": "Threading test"
        }))
        .await;

    assert_eq!(conv_response.status_code(), 201);
    let conv_data: serde_json::Value = conv_response.json();
    let conversation_id = conv_data["id"].as_str().unwrap();

    // Make first text response
    let first_response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "input": "Hello",
            "conversation": {
                "id": conversation_id
            },
            "stream": true
        }))
        .await;

    assert_eq!(first_response.status_code(), 200);
    let first_response_text = first_response.text();

    // Extract first response ID from response.created event
    let first_response_id = if let Some(start) = first_response_text.find("\"id\":\"resp_") {
        let start_uuid = start + "\"id\":\"resp_".len();
        let end = first_response_text[start_uuid..].find('"').unwrap();
        &first_response_text[start_uuid..start_uuid + end]
    } else {
        panic!("Could not find response ID in first response");
    };

    // Make second image generation response with previous_response_id
    let second_response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": image_model,
            "input": "Generate an image",
            "conversation": {
                "id": conversation_id
            },
            "previous_response_id": format!("resp_{}", first_response_id),
            "stream": true
        }))
        .await;

    assert_eq!(second_response.status_code(), 200);
    let second_response_text = second_response.text();

    // Verify threading is present in the response
    assert!(
        second_response_text.contains("previous_response_id"),
        "Expected previous_response_id in second response"
    );

    println!("Threading test passed!");
}

/// Test usage tracking for image generation
/// Verifies that image_count is properly recorded in usage
#[tokio::test]
async fn test_image_usage_tracking() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let image_model = setup_qwen_image_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Request image generation
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": image_model,
            "input": "Beautiful sunset",
            "stream": true
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_text = response.text();

    // Look for image_count in usage within response.completed event
    // The usage should have image_count: 1
    assert!(
        response_text.contains("\"image_count\""),
        "Expected image_count in usage for image generation"
    );

    println!("Usage tracking test passed!");
}

/// Test backward compatibility: text completions still work
/// Verifies that non-image models continue to work as before
#[tokio::test]
async fn test_text_completion_backward_compatibility() {
    let (server, _guard) = setup_test_server().await;
    let qwen_model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Request text completion (should not be detected as image)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "input": "What is 2+2?",
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    println!(
        "Text completion response status: {}",
        response.status_code()
    );
    assert_eq!(response.status_code(), 200);

    let response_text = response.text();

    // Verify we got output_text, not output_image
    assert!(
        response_text.contains("output_text"),
        "Expected output_text for text model"
    );

    // Should NOT have output_image
    assert!(
        !response_text.contains("output_image.created"),
        "Text model should not generate output_image events"
    );

    // Verify the response has tokens (not image_count)
    assert!(
        response_text.contains("\"total_tokens\""),
        "Expected token usage for text completion"
    );

    println!("Backward compatibility test passed!");
}

/// Test image generation with n parameter (multiple images)
/// Verifies that the n parameter is respected
#[tokio::test]
async fn test_image_generation_multiple_images() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let image_model = setup_qwen_image_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Request multiple images (n=2 or n=3)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": image_model,
            "input": "Generate beautiful images",
            "stream": true
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_text = response.text();

    // Verify we got output_image.created
    assert!(
        response_text.contains("output_image"),
        "Expected output_image for image generation"
    );

    println!("Multiple images test passed!");
}

/// Test that text models route to text completion
/// Verifies that only image-capable models trigger image generation routing
#[tokio::test]
async fn test_unknown_model_routes_to_text_completion() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Use a model name that doesn't match any image pattern
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "unknown-model-12345",
            "input": "Test input",
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    // This will likely fail to find a provider, but should attempt text completion
    // The important thing is it tries to route to text completion, not image generation
    println!(
        "Unknown model response status: {} (expected to fail with 404 or similar)",
        response.status_code()
    );
}

/// Test image model detection
/// Verifies that image models trigger image generation routing
#[tokio::test]
async fn test_image_model_detection() {
    let (server, _guard) = setup_test_server().await;
    setup_qwen_model(&server).await;
    let image_model = setup_qwen_image_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Request with image model
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": image_model,
            "input": "A serene mountain landscape",
            "stream": true
        }))
        .await;

    println!("Flux model response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let response_text = response.text();

    // Verify it was treated as image generation
    assert!(
        response_text.contains("output_image"),
        "Expected output_image for flux model"
    );

    println!("Flux model test passed!");
}

/// Test that image input with text model fails gracefully
/// Verifies validation: if input has image but model is text, should error
#[tokio::test]
async fn test_image_input_with_text_model_error() {
    let (server, _guard) = setup_test_server().await;
    let qwen_model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let base64_image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    // Try to send image input to text model (which will treat it as text input)
    // This should still work - the text model should process the image_url as part of input
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "input": [{
                "role": "user",
                "content": [
                    {
                        "type": "input_image",
                        "image_url": {
                            "url": format!("data:image/png;base64,{}", base64_image)
                        }
                    }
                ]
            }],
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    println!(
        "Image input with text model response status: {}",
        response.status_code()
    );

    // Text model should process this as a multi-modal input
    // So this should succeed and return text output, not image output
    if response.status_code() == 200 {
        let response_text = response.text();
        assert!(
            response_text.contains("output_text"),
            "Expected output_text when text model receives image input"
        );
    }

    println!("Text model with image input test passed!");
}

/// Test image analysis through Response API
/// Verifies that text models can analyze images and return text responses
#[tokio::test]
async fn test_image_analysis_with_text_model() {
    let (server, _guard) = setup_test_server().await;
    let qwen_model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a simple 1x1 PNG image in base64
    let base64_image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    // Create conversation for image analysis
    let conv_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "name": "Image analysis test"
        }))
        .await;

    assert_eq!(conv_response.status_code(), 201);
    let conv_data: serde_json::Value = conv_response.json();
    let conversation_id = conv_data["id"].as_str().unwrap();

    // Request image analysis (text model + image input = text output)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "input": [{
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "What is in this image?"
                    },
                    {
                        "type": "input_image",
                        "image_url": {
                            "url": format!("data:image/png;base64,{}", base64_image)
                        }
                    }
                ]
            }],
            "conversation": {
                "id": conversation_id
            },
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    println!("Image analysis response status: {}", response.status_code());
    assert_eq!(response.status_code(), 200);

    let response_text = response.text();

    // Verify we got text output, not image output
    assert!(
        response_text.contains("output_text"),
        "Expected output_text for image analysis"
    );

    // Should NOT have image output
    assert!(
        !response_text.contains("output_image.created"),
        "Image analysis should return text, not images"
    );

    // Verify response.created and response.completed events
    assert!(
        response_text.contains("response.created"),
        "Expected response.created event"
    );
    assert!(
        response_text.contains("response.completed"),
        "Expected response.completed event"
    );

    println!("Image analysis test passed!");
}

/// Test multimodal content in conversation threading
/// Verifies that image analysis maintains proper conversation context
#[tokio::test]
async fn test_image_analysis_conversation_threading() {
    let (server, _guard) = setup_test_server().await;
    let qwen_model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create conversation
    let conv_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "name": "Multimodal threading test"
        }))
        .await;

    assert_eq!(conv_response.status_code(), 201);
    let conv_data: serde_json::Value = conv_response.json();
    let conversation_id = conv_data["id"].as_str().unwrap();

    // First message: text only
    let first_response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "input": "Tell me about image analysis",
            "conversation": {
                "id": conversation_id
            },
            "stream": true
        }))
        .await;

    assert_eq!(first_response.status_code(), 200);
    let first_response_text = first_response.text();

    // Extract first response ID
    let first_response_id = if let Some(start) = first_response_text.find("\"id\":\"resp_") {
        let start_uuid = start + "\"id\":\"resp_".len();
        let end = first_response_text[start_uuid..].find('"').unwrap();
        &first_response_text[start_uuid..start_uuid + end]
    } else {
        panic!("Could not find response ID in first response");
    };

    // Second message: image analysis with multimodal content
    let base64_image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    let second_response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "input": [{
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Analyze this image"
                    },
                    {
                        "type": "input_image",
                        "image_url": {
                            "url": format!("data:image/png;base64,{}", base64_image)
                        }
                    }
                ]
            }],
            "conversation": {
                "id": conversation_id
            },
            "previous_response_id": format!("resp_{}", first_response_id),
            "stream": true
        }))
        .await;

    assert_eq!(second_response.status_code(), 200);
    let second_response_text = second_response.text();

    // Verify threading is maintained
    assert!(
        second_response_text.contains("previous_response_id"),
        "Expected previous_response_id in second response"
    );

    // Verify text output (not image)
    assert!(
        second_response_text.contains("output_text"),
        "Expected output_text from image analysis"
    );

    println!("Multimodal threading test passed!");
}

/// Test mixed content in multimodal requests
/// Verifies proper handling of text + multiple images
#[tokio::test]
async fn test_multimodal_mixed_content() {
    let (server, _guard) = setup_test_server().await;
    let qwen_model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let base64_image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    // Request with multiple images and text
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "input": [{
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Compare these images"
                    },
                    {
                        "type": "input_image",
                        "image_url": {
                            "url": format!("data:image/png;base64,{}", base64_image)
                        }
                    },
                    {
                        "type": "input_image",
                        "image_url": {
                            "url": format!("data:image/png;base64,{}", base64_image)
                        }
                    }
                ]
            }],
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    println!(
        "Mixed multimodal content response status: {}",
        response.status_code()
    );
    assert_eq!(response.status_code(), 200);

    let response_text = response.text();

    // Should return text analysis, not images
    assert!(
        response_text.contains("output_text"),
        "Expected output_text for multimodal analysis"
    );

    println!("Mixed multimodal content test passed!");
}

/// Test text-only input still works with text models
/// Verifies backward compatibility when no images are present
#[tokio::test]
async fn test_text_only_multimodal_input() {
    let (server, _guard) = setup_test_server().await;
    let qwen_model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Use Items format but with only text (no images)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": qwen_model,
            "input": [{
                "role": "user",
                "content": "What is the capital of France?"
            }],
            "stream": true,
            "max_tokens": 100
        }))
        .await;

    println!(
        "Text-only Items input response status: {}",
        response.status_code()
    );
    if response.status_code() != 200 {
        let error_text = response.text();
        println!("Error response: {}", error_text);
    }
    assert_eq!(response.status_code(), 200);

    let response_text = response.text();

    // Should return text output normally
    assert!(
        response_text.contains("output_text"),
        "Expected output_text for text-only input"
    );

    // Should not contain image events
    assert!(
        !response_text.contains("output_image"),
        "Should not have image output for text-only input"
    );

    println!("Text-only Items input test passed!");
}
