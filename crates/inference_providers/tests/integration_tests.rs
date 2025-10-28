//! Integration tests for the vLLM provider
//!
//! These tests require a running vLLM instance and will make real HTTP requests.
//! Run with: `cargo test --test integration_tests -- --nocapture`

use futures_util::StreamExt;
use inference_providers::{
    ChatCompletionParams, ChatMessage, CompletionParams, InferenceProvider, MessageRole,
    StreamChunk, VLlmConfig, VLlmProvider,
};
use std::time::Duration;
use tokio::time::timeout;

/// Get vLLM base URL from environment variable or use default
fn get_vllm_base_url() -> String {
    std::env::var("VLLM_BASE_URL").unwrap_or_else(|_| "http://localhost:8002".to_string())
}

/// Get vLLM API key from environment variable
fn get_vllm_api_key() -> Option<String> {
    std::env::var("VLLM_API_KEY").ok()
}

/// Get test timeout from environment variable or use default
fn get_test_timeout_secs() -> u64 {
    std::env::var("VLLM_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

/// Create a configured vLLM provider for testing
fn create_test_provider() -> VLlmProvider {
    let _ = dotenvy::dotenv().unwrap();
    let config = VLlmConfig {
        base_url: get_vllm_base_url(),
        api_key: get_vllm_api_key(),
        timeout_seconds: get_test_timeout_secs() as i64,
    };
    VLlmProvider::new(config)
}

#[tokio::test]
async fn test_models_endpoint() {
    let provider = create_test_provider();
    let test_timeout_secs = get_test_timeout_secs();

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
    let test_timeout_secs = get_test_timeout_secs();

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
                content: Some("You are a helpful assistant. Please respond briefly.".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some("Hello! Can you count to 3?".to_string()),
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
        response_format: None,
        seed: None,
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        metadata: None,
        store: None,
        stream_options: None,
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
    let test_timeout_secs = get_test_timeout_secs();

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
            content: Some("Hello".to_string()),
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
        response_format: None,
        seed: None,
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        metadata: None,
        store: None,
        stream_options: None,
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
    // Test with different configurations
    let _ = dotenvy::dotenv().unwrap();
    let config = VLlmConfig {
        base_url: get_vllm_base_url(),
        api_key: get_vllm_api_key(),
        timeout_seconds: 10,
    };

    let provider = VLlmProvider::new(config.clone());

    // Test that the provider was created successfully
    let models = provider.models().await;
    assert!(models.is_ok(), "Provider should work with valid config");
}
