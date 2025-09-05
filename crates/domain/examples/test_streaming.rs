use domain::providers::vllm::VLlmProvider;
use domain::providers::{CompletionProvider};
use domain::models::{ChatCompletionParams, CompletionParams, ChatMessage, MessageRole};
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for better debugging (optional, requires tracing-subscriber dependency)
    // tracing_subscriber::fmt::init();

    // Configuration - change these to match your vLLM server
    let base_url = std::env::var("VLLM_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:8000".to_string());
    let api_key = std::env::var("VLLM_API_KEY").ok();
    let model_id = std::env::var("MODEL_ID")
        .unwrap_or_else(|_| "meta-llama/Llama-2-7b-hf".to_string());

    println!("Testing token streaming with:");
    println!("  Base URL: {}", base_url);
    println!("  Model ID: {}", model_id);
    println!("  API Key: {}", if api_key.is_some() { "Set" } else { "Not set" });
    println!();

    // Create the vLLM provider
    let provider = VLlmProvider::new(
        "vllm".to_string(),
        base_url.clone(),
        api_key.clone(),
    );

    // Test 1: Chat completion streaming
    println!("=== Testing Chat Completion Streaming ===");
    let chat_params = ChatCompletionParams {
        model_id: model_id.clone(),
        messages: vec![
            ChatMessage {
                role: MessageRole::System,
                content: "You are a helpful assistant.".to_string(),
                name: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: "Write a haiku about streaming data.".to_string(),
                name: None,
            },
        ],
        max_tokens: Some(100),
        temperature: Some(0.7),
        top_p: None,
        stop_sequences: None,
        stream: Some(true),
    };

    match provider.chat_completion_stream(chat_params).await {
        Ok(mut stream) => {
            println!("Streaming response:");
            print!("  ");
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        // Process each chunk
                        for choice in &chunk.choices {
                            if let Some(content) = &choice.delta.content {
                                print!("{}", content);
                                // Flush to see tokens as they arrive
                                use std::io::Write;
                                std::io::stdout().flush()?;
                            }
                            if let Some(finish_reason) = &choice.finish_reason {
                                println!();
                                println!("  Finish reason: {}", finish_reason);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("\n  Error in stream: {:?}", e);
                        break;
                    }
                }
            }
            println!();
        }
        Err(e) => {
            eprintln!("Failed to start chat streaming: {:?}", e);
            eprintln!("Make sure your vLLM server is running at {}", base_url);
        }
    }

    // Test 2: Text completion streaming
    println!("\n=== Testing Text Completion Streaming ===");
    let text_params = CompletionParams {
        model_id: model_id.clone(),
        prompt: "The future of artificial intelligence is".to_string(),
        max_tokens: Some(50),
        temperature: Some(0.7),
        top_p: None,
        stop_sequences: None,
        stream: Some(true),
    };

    match provider.text_completion_stream(text_params).await {
        Ok(mut stream) => {
            println!("Streaming response:");
            print!("  ");
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        for choice in &chunk.choices {
                            if let Some(content) = &choice.delta.content {
                                print!("{}", content);
                                use std::io::Write;
                                std::io::stdout().flush()?;
                            }
                            if let Some(finish_reason) = &choice.finish_reason {
                                println!();
                                println!("  Finish reason: {}", finish_reason);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("\n  Error in stream: {:?}", e);
                        break;
                    }
                }
            }
            println!();
        }
        Err(e) => {
            eprintln!("Failed to start text streaming: {:?}", e);
            eprintln!("Make sure your vLLM server is running at {}", base_url);
        }
    }

    Ok(())
}
