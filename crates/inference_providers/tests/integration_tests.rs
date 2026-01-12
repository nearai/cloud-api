//! Integration tests for the inference provider
//!
//! These tests use MockProvider by default to avoid external dependencies.
//! Set USE_REAL_VLLM=true to use the real VLLM provider instead.
//! Run with: `cargo test --test integration_tests -- --nocapture`

use futures_util::StreamExt;
use inference_providers::{
    mock::{RequestMatcher, ResponseTemplate},
    ChatCompletionParams, ChatMessage, CompletionParams, FunctionDefinition, InferenceProvider,
    MessageRole, MockProvider, StreamChunk, ToolChoice, ToolDefinition,
};
use std::time::Duration;
use tokio::time::timeout;

/// Create a mock provider for testing
///
/// Uses MockProvider by default to avoid external dependencies.
/// Set USE_REAL_VLLM=true to use the real VLLM provider instead.
fn create_test_provider() -> Box<dyn InferenceProvider> {
    if std::env::var("USE_REAL_VLLM").is_ok() {
        // Use real VLLM provider if explicitly requested
        use inference_providers::{VLlmConfig, VLlmProvider};
        let _ = dotenvy::dotenv();
        let config = VLlmConfig {
            base_url: std::env::var("VLLM_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8002".to_string()),
            api_key: std::env::var("VLLM_API_KEY").ok(),
            timeout_seconds: std::env::var("VLLM_TEST_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30) as i64,
        };
        Box::new(VLlmProvider::new(config))
    } else {
        // Use mock provider by default
        Box::new(MockProvider::new())
    }
}

#[tokio::test]
async fn test_models_endpoint() {
    let provider = create_test_provider();
    let test_timeout_secs = 30;

    let result = timeout(Duration::from_secs(test_timeout_secs), provider.models()).await;

    match result {
        Ok(Ok(models)) => {
            assert!(
                !models.data.is_empty(),
                "Should have at least one model available"
            );
            println!("Available models: {:#?}", models.data);

            // Verify model structure
            for model in &models.data {
                assert!(!model.id.is_empty(), "Model ID should not be empty");
                assert!(!model.object.is_empty(), "Model object should not be empty");
                assert!(model.created > 0, "Model created should be greater than 0");
            }
        }
        Ok(Err(e)) => panic!("Models request failed: {e}"),
        Err(_) => panic!("Models request timed out after {test_timeout_secs} seconds"),
    }
}

#[tokio::test]
async fn test_chat_completion_streaming() {
    let provider = create_test_provider();
    let test_timeout_secs = 30;

    // First get available models
    let models = provider.models().await.expect("Failed to get models");
    assert!(!models.data.is_empty(), "No models available for testing");

    let model_id = &models.data[0].id;
    println!("Testing with model: {model_id}");

    let params = ChatCompletionParams {
        model: model_id.clone(),
        messages: vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(serde_json::Value::String(
                    "You are a helpful assistant. Please respond briefly.".to_string(),
                )),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::Value::String(
                    "Hello! Can you count to 3?".to_string(),
                )),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ],
        max_completion_tokens: Some(50),
        temperature: Some(0.7),
        stream: Some(true),
        max_tokens: None,
        top_p: None,
        n: None,
        stop: None,
        frequency_penalty: None,
        presence_penalty: None,
        logit_bias: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        seed: None,
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        metadata: None,
        store: None,
        stream_options: None,
        modalities: None,
        extra: std::collections::HashMap::new(),
    };

    let stream_result = timeout(
        Duration::from_secs(test_timeout_secs),
        provider.chat_completion_stream(params, "test_request_hash".to_string()),
    )
    .await;

    match stream_result {
        Ok(Ok(mut stream)) => {
            let mut chunks_received = 0;
            let mut content_received = String::new();
            let mut usage_found = false;

            // Process streaming chunks
            while let Some(chunk_result) = timeout(Duration::from_secs(5), stream.next())
                .await
                .unwrap_or(None)
            {
                match chunk_result {
                    Ok(sse_event) => match sse_event.chunk {
                        StreamChunk::Chat(chat_chunk) => {
                            chunks_received += 1;
                            println!("Received chat chunk #{chunks_received}: {chat_chunk:?}");

                            // Check for token usage in final chunk
                            if let Some(usage) = &chat_chunk.usage {
                                usage_found = true;
                                assert!(
                                    usage.total_tokens > 0,
                                    "Total tokens should be greater than 0"
                                );
                                assert!(
                                    usage.prompt_tokens > 0,
                                    "Prompt tokens should be greater than 0"
                                );
                                assert!(
                                    usage.completion_tokens > 0,
                                    "Completion tokens should be greater than 0"
                                );
                                println!("Token usage: {usage:?}");
                            }

                            // Collect content from deltas
                            if let Some(choice) = chat_chunk.choices.first() {
                                if let Some(delta) = &choice.delta {
                                    if let Some(content) = &delta.content {
                                        content_received.push_str(content);
                                    }
                                }
                            }
                        }
                        StreamChunk::Text(text_chunk) => {
                            panic!("CRITICAL ERROR: Received text chunk in chat completion stream! This indicates stream isolation failure. Chunk: {text_chunk:?}");
                        }
                    },
                    Err(e) => {
                        // Stream errors should be treated as test failures
                        panic!("Stream error in chat completion: {e}. This could indicate SSE parsing issues or stream corruption.");
                    }
                }

                // Safety limit to avoid infinite loops
                if chunks_received > 100 {
                    break;
                }
            }

            assert!(
                chunks_received > 0,
                "Should have received at least one chunk"
            );
            assert!(usage_found, "Should have received token usage information");
            println!("Total content received: '{content_received}'");
            println!("Successfully processed {chunks_received} chunks with token usage enabled");

            // Verify we received meaningful content
            assert!(!content_received.is_empty() || chunks_received >= 2,
                    "Should receive content or at least 2 chunks (initial + usage). Got {chunks_received} chunks with content: '{content_received}'");
        }
        Ok(Err(e)) => panic!("Chat completion failed: {e}"),
        Err(_) => panic!("Chat completion timed out after {test_timeout_secs} seconds"),
    }
}

#[tokio::test]
async fn test_text_completion_streaming() {
    let provider = create_test_provider();
    let test_timeout_secs = 30;

    // First get available models
    let models = provider.models().await.expect("Failed to get models");
    assert!(!models.data.is_empty(), "No models available for testing");

    let model_id = &models.data[0].id;
    println!("Testing text completion with model: {model_id}");

    let params = CompletionParams {
        model: model_id.clone(),
        prompt: "The capital of France is".to_string(),
        max_tokens: Some(20),
        temperature: Some(0.3),
        stream: Some(true),
        top_p: None,
        n: None,
        stop: None,
        frequency_penalty: None,
        presence_penalty: None,
        logit_bias: None,
        logprobs: None,
        echo: None,
        best_of: None,
        seed: None,
        user: None,
        suffix: None,
        stream_options: None,
    };

    let stream_result = timeout(
        Duration::from_secs(test_timeout_secs),
        provider.text_completion_stream(params),
    )
    .await;

    match stream_result {
        Ok(Ok(mut stream)) => {
            let mut chunks_received = 0;
            let mut content_received = String::new();
            let mut usage_found = false;

            // Process streaming chunks
            while let Some(chunk_result) = timeout(Duration::from_secs(5), stream.next())
                .await
                .unwrap_or(None)
            {
                match chunk_result {
                    Ok(sse_event) => match sse_event.chunk {
                        StreamChunk::Text(text_chunk) => {
                            chunks_received += 1;
                            println!("Received text chunk #{chunks_received}: {text_chunk:?}");

                            // Check for token usage in final chunk
                            if let Some(usage) = &text_chunk.usage {
                                usage_found = true;
                                assert!(
                                    usage.total_tokens > 0,
                                    "Total tokens should be greater than 0"
                                );
                                assert!(
                                    usage.prompt_tokens > 0,
                                    "Prompt tokens should be greater than 0"
                                );
                                assert!(
                                    usage.completion_tokens > 0,
                                    "Completion tokens should be greater than 0"
                                );
                                println!("Token usage: {usage:?}");
                            }

                            // Collect content from choices
                            if let Some(choice) = text_chunk.choices.first() {
                                content_received.push_str(&choice.text);
                            }
                        }
                        StreamChunk::Chat(chat_chunk) => {
                            panic!("CRITICAL ERROR: Received chat chunk in text completion stream! This indicates stream isolation failure. Chunk: {chat_chunk:?}");
                        }
                    },
                    Err(e) => {
                        panic!("Stream error in text completion: {e}. This could indicate SSE parsing issues or stream corruption.");
                    }
                }

                // Safety limit to avoid infinite loops
                if chunks_received > 100 {
                    break;
                }
            }

            assert!(
                chunks_received > 0,
                "Should have received at least one chunk"
            );
            assert!(usage_found, "Should have received token usage information");
            assert!(
                !content_received.is_empty(),
                "Should have received some content"
            );
            println!("Total content received: '{content_received}'");
        }
        Ok(Err(e)) => panic!("Text completion failed: {e}"),
        Err(_) => panic!("Text completion timed out after {test_timeout_secs} seconds"),
    }
}

#[tokio::test]
async fn test_error_handling() {
    // Test with invalid model
    let provider = create_test_provider();

    let params = ChatCompletionParams {
        model: "nonexistent-model-12345".to_string(),
        messages: vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::Value::String("Hello".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        max_completion_tokens: Some(10),
        max_tokens: None,
        temperature: None,
        top_p: None,
        n: None,
        stream: None,
        stop: None,
        frequency_penalty: None,
        presence_penalty: None,
        logit_bias: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        seed: None,
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        metadata: None,
        store: None,
        stream_options: None,
        modalities: None,
        extra: std::collections::HashMap::new(),
    };

    let result = provider
        .chat_completion_stream(params, "test_request_hash".to_string())
        .await;
    assert!(result.is_err(), "Should fail with invalid model");

    if let Err(e) = result {
        println!("Expected error for invalid model: {e}");
    }
}

#[tokio::test]
async fn test_configuration() {
    // Test that the mock provider works correctly
    let provider = MockProvider::new();
    let models = provider.models().await;
    assert!(models.is_ok(), "Mock provider should work correctly");
    assert!(
        !models.unwrap().data.is_empty(),
        "Mock provider should return models"
    );
}

#[tokio::test]
async fn test_chat_completion_streaming_with_tool_calls() {
    let provider = create_test_provider();
    let test_timeout_secs = 30;

    // First get available models
    let models = provider.models().await.expect("Failed to get models");
    assert!(!models.data.is_empty(), "No models available for testing");

    let model_id = &models.data[0].id;
    println!("Testing tool calls with model: {model_id}");

    // Create the tool definition for get_weather
    let weather_params = serde_json::json!({
        "type": "object",
        "properties": {
            "location": {
                "type": "string",
                "description": "City name"
            }
        },
        "required": ["location"]
    });

    let tools = vec![ToolDefinition {
        type_: "function".to_string(),
        function: FunctionDefinition {
            name: "get_weather".to_string(),
            description: Some("Get the current weather for a city".to_string()),
            parameters: weather_params,
        },
    }];

    let params = ChatCompletionParams {
        model: model_id.clone(),
        messages: vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::Value::String(
                "What's the weather in New York today?".to_string(),
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        max_completion_tokens: Some(100),
        temperature: Some(0.7),
        stream: Some(true),
        max_tokens: None,
        top_p: None,
        n: None,
        stop: None,
        frequency_penalty: None,
        presence_penalty: None,
        logit_bias: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        seed: None,
        tools: Some(tools),
        tool_choice: Some(ToolChoice::String("auto".to_string())),
        parallel_tool_calls: None,
        metadata: None,
        store: None,
        stream_options: None,
        modalities: None,
        extra: std::collections::HashMap::new(),
    };

    let stream_result = timeout(
        Duration::from_secs(test_timeout_secs),
        provider.chat_completion_stream(params, "test_tool_call_request_hash".to_string()),
    )
    .await;

    match stream_result {
        Ok(Ok(mut stream)) => {
            let mut chunks_received = 0;
            let mut content_received = String::new();
            let mut tool_calls_found = false;
            let mut tool_call_ids = Vec::new();
            let mut tool_call_names = Vec::new();
            let mut tool_call_arguments = std::collections::HashMap::new();

            // Process streaming chunks
            while let Some(chunk_result) = timeout(Duration::from_secs(5), stream.next())
                .await
                .unwrap_or(None)
            {
                match chunk_result {
                    Ok(sse_event) => match sse_event.chunk {
                        StreamChunk::Chat(chat_chunk) => {
                            chunks_received += 1;

                            // Check for tool calls in delta
                            if let Some(choice) = chat_chunk.choices.first() {
                                if let Some(delta) = &choice.delta {
                                    // Collect content
                                    if let Some(content) = &delta.content {
                                        content_received.push_str(content);
                                    }

                                    // Check for tool calls
                                    if let Some(tool_calls) = &delta.tool_calls {
                                        tool_calls_found = true;
                                        println!(
                                            "Received tool call delta in chunk #{chunks_received}: {tool_calls:?}"
                                        );

                                        for tool_call_delta in tool_calls {
                                            // Track tool call IDs
                                            if let Some(id) = &tool_call_delta.id {
                                                if !tool_call_ids.contains(id) {
                                                    tool_call_ids.push(id.clone());
                                                    println!("Tool call ID: {id}");
                                                }
                                            }

                                            // Track tool call names
                                            if let Some(function) = &tool_call_delta.function {
                                                if let Some(name) = &function.name {
                                                    if !tool_call_names.contains(name) {
                                                        tool_call_names.push(name.clone());
                                                        println!("Tool call function name: {name}");
                                                    }
                                                }

                                                // Accumulate arguments (they come in chunks)
                                                if let Some(arguments_delta) = &function.arguments {
                                                    if let Some(index) = tool_call_delta.index {
                                                        let entry = tool_call_arguments
                                                            .entry(index)
                                                            .or_insert_with(String::new);
                                                        entry.push_str(arguments_delta);
                                                    }
                                                }
                                            }

                                            // Log index if present
                                            if let Some(index) = tool_call_delta.index {
                                                println!("Tool call index: {index}");
                                            }
                                        }
                                    }
                                }
                            }

                            // Log chunk details periodically
                            if chunks_received % 10 == 0 {
                                println!("Processed {chunks_received} chunks so far...");
                            }
                        }
                        StreamChunk::Text(text_chunk) => {
                            panic!("CRITICAL ERROR: Received text chunk in chat completion stream with tool calls! Chunk: {text_chunk:?}");
                        }
                    },
                    Err(e) => {
                        // Stream errors should be treated as test failures
                        panic!("Stream error in tool call chat completion: {e}. This could indicate SSE parsing issues with ToolCallDelta or stream corruption.");
                    }
                }

                // Safety limit to avoid infinite loops
                if chunks_received > 200 {
                    break;
                }
            }

            assert!(
                chunks_received > 0,
                "Should have received at least one chunk"
            );

            println!("Total chunks received: {chunks_received}");
            println!("Content received: '{content_received}'");
            println!("Tool calls found: {tool_calls_found}");
            println!("Tool call IDs: {tool_call_ids:?}");
            println!("Tool call function names: {tool_call_names:?}");
            println!("Tool call arguments (by index): {tool_call_arguments:?}");

            // Verify that tool calls were received (if the model supports them)
            // Note: Some models may not call tools for this specific query, so we log but don't fail
            if tool_calls_found {
                assert!(
                    !tool_call_ids.is_empty(),
                    "If tool calls were found, should have at least one tool call ID"
                );
                assert!(
                    !tool_call_names.is_empty(),
                    "If tool calls were found, should have at least one function name"
                );
                // Verify that we got the expected tool call
                assert!(
                    tool_call_names.contains(&"get_weather".to_string()),
                    "Should have received get_weather tool call"
                );
                println!("✓ Successfully received and parsed tool calls!");
            } else {
                println!("⚠ No tool calls received - this may be expected if the model chose not to call tools");
            }
        }
        Ok(Err(e)) => panic!("Tool call chat completion failed: {e}"),
        Err(_) => panic!("Tool call chat completion timed out after {test_timeout_secs} seconds"),
    }
}

#[tokio::test]
async fn test_reasoning_content() {
    // Only test with MockProvider as VLLM models might not support/return reasoning
    if std::env::var("USE_REAL_VLLM").is_ok() {
        println!("Skipping reasoning content test for real VLLM");
        return;
    }

    let provider = MockProvider::new();

    // Setup expectation with reasoning
    provider
        .when(RequestMatcher::ExactPrompt(
            "Why is the sky blue?".to_string(),
        ))
        .respond_with(
            ResponseTemplate::new("The sky is blue due to Rayleigh scattering.")
                .with_reasoning("I should explain Rayleigh scattering simply."),
        )
        .await;

    let params = ChatCompletionParams {
        model: "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        messages: vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::Value::String(
                "Why is the sky blue?".to_string(),
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        max_completion_tokens: Some(100),
        temperature: Some(0.7),
        stream: Some(true),
        max_tokens: None,
        top_p: None,
        n: None,
        stop: None,
        frequency_penalty: None,
        presence_penalty: None,
        logit_bias: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        seed: None,
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        metadata: None,
        store: None,
        stream_options: None,
        modalities: None,
        extra: std::collections::HashMap::new(),
    };

    let stream = provider
        .chat_completion_stream(params, "test_hash".to_string())
        .await
        .expect("Failed to create stream");

    let mut reasoning_received = String::new();
    let mut content_received = String::new();
    let mut stream = stream;

    while let Some(chunk_result) = stream.next().await {
        let event = chunk_result.expect("Stream error");
        match event.chunk {
            StreamChunk::Chat(chunk) => {
                if let Some(choice) = chunk.choices.first() {
                    if let Some(delta) = &choice.delta {
                        if let Some(reasoning) = &delta.reasoning_content {
                            reasoning_received.push_str(reasoning);
                        }
                        if let Some(content) = &delta.content {
                            content_received.push_str(content);
                        }
                    }
                }
            }
            _ => panic!("Unexpected chunk type"),
        }
    }

    assert_eq!(
        reasoning_received,
        "I should explain Rayleigh scattering simply."
    );
    assert_eq!(
        content_received,
        "The sky is blue due to Rayleigh scattering."
    );
}

/// Test image generation with real vLLM provider
/// Run with: VLLM_BASE_URL=<your-url> VLLM_API_KEY=<your-key> VLLM_IMAGE_MODEL=<model-id> cargo test test_image_generation_real -- --nocapture --ignored
#[tokio::test]
#[ignore] // Only run when explicitly requested (requires real vLLM server)
async fn test_image_generation_real() {
    use inference_providers::{ImageGenerationParams, VLlmConfig, VLlmProvider};

    let _ = dotenvy::dotenv();

    let base_url = std::env::var("VLLM_BASE_URL").expect("VLLM_BASE_URL must be set for this test");
    let model =
        std::env::var("VLLM_IMAGE_MODEL").unwrap_or_else(|_| "test-image-model".to_string());

    let config = VLlmConfig {
        base_url,
        api_key: std::env::var("VLLM_API_KEY").ok(),
        timeout_seconds: 120, // Image generation can take longer
    };
    let provider = VLlmProvider::new(config);

    let params = ImageGenerationParams {
        model,
        prompt: "A cute baby sea otter swimming in blue water".to_string(),
        n: Some(1),
        size: Some("1024x1024".to_string()),
        response_format: Some("b64_json".to_string()),
        quality: None,
        style: None,
    };

    println!("Starting image generation request...");
    let result = provider
        .image_generation(params, "test-request-hash".to_string())
        .await;

    match result {
        Ok(response) => {
            println!("Image generation successful!");
            println!("Response ID: {}", response.id);
            println!("Created at: {}", response.created);
            println!("Number of images: {}", response.data.len());

            for (i, img) in response.data.iter().enumerate() {
                if let Some(b64) = &img.b64_json {
                    println!("Image {}: base64 data length = {} bytes", i, b64.len());
                    assert!(!b64.is_empty(), "Base64 data should not be empty");
                }
                if let Some(url) = &img.url {
                    println!("Image {}: URL = {}", i, url);
                }
                if let Some(revised) = &img.revised_prompt {
                    println!("Image {}: Revised prompt = {}", i, revised);
                }
            }

            assert!(!response.data.is_empty(), "Should have at least one image");
        }
        Err(e) => {
            panic!("Image generation failed: {}", e);
        }
    }
}

/// Test image generation with mock provider
#[tokio::test]
async fn test_image_generation_mock() {
    use inference_providers::{ImageGenerationParams, MockProvider};

    // Use new_accept_all() to accept any model name
    let provider = MockProvider::new_accept_all();

    let params = ImageGenerationParams {
        model: "mock-image-model".to_string(),
        prompt: "A beautiful sunset over mountains".to_string(),
        n: Some(1),
        size: Some("512x512".to_string()),
        response_format: Some("b64_json".to_string()),
        quality: None,
        style: None,
    };

    let result = provider
        .image_generation(params, "test-request-hash".to_string())
        .await;

    match result {
        Ok(response) => {
            println!("Mock image generation successful!");
            assert!(!response.data.is_empty(), "Should have at least one image");
            assert!(
                response.data[0].b64_json.is_some(),
                "Should have base64 data"
            );
        }
        Err(e) => {
            panic!("Mock image generation failed: {}", e);
        }
    }
}
