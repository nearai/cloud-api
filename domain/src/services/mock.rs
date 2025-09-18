use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use crate::{
    errors::CompletionError,
    models::*,
    providers::StreamChunk,
    services::{CompletionHandler, TdxHandler},
};

/// Mock completion service for testing and development
pub struct MockCompletionHandler;

#[async_trait]
impl CompletionHandler for MockCompletionHandler {
    fn name(&self) -> &str {
        "mock-completion-handler"
    }
    
    fn supports_model(&self, model_id: &str) -> bool {
        !model_id.is_empty()
    }
    async fn chat_completion(&self, params: ChatCompletionParams) -> Result<ChatCompletionResult, CompletionError> {
        // Validate model
        if params.model_id.is_empty() {
            return Err(CompletionError::InvalidModel("Model ID cannot be empty".to_string()));
        }

        // Validate messages
        if params.messages.is_empty() {
            return Err(CompletionError::InvalidParams("Messages cannot be empty".to_string()));
        }

        // Get the last user message to respond to
        let last_message = params.messages.last().unwrap();
        let message_content = last_message.content.as_ref()
            .map(|s| s.as_str())
            .unwrap_or("empty");
        let response_content = match &message_content.to_lowercase() {
            content if content.contains("hello") => "Hello! How can I assist you today?".to_string(),
            content if content.contains("weather") => "I don't have access to real-time weather data, but I'd be happy to help with other questions!".to_string(),
            content if content.contains("help") => "I'm here to help! Please let me know what you need assistance with.".to_string(),
            _ => format!("I understand you're asking about: '{}'. This is a mock response from the completion service.", message_content),
        };

        // Mock token calculation (simple heuristic)
        let prompt_tokens = params.messages.iter()
            .map(|m| m.content.as_ref().map_or(0, |c| c.split_whitespace().count()) as u32)
            .sum::<u32>() + 10; // +10 for system overhead
        let completion_tokens = response_content.split_whitespace().count() as u32;

        Ok(ChatCompletionResult {
            message: ChatMessage {
                role: MessageRole::Assistant,
                content: Some(response_content),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: FinishReason::Stop,
            usage: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            },
        })
    }

    async fn chat_completion_stream(&self, params: ChatCompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        use futures::stream;
        use crate::providers::{StreamChunk, StreamChoice, Delta};
        
        // Validate model
        if params.model_id.is_empty() {
            return Err(CompletionError::InvalidModel("Model ID cannot be empty".to_string()));
        }
        
        // Create mock streaming response
        let chunks = vec![
            StreamChunk {
                id: "chatcmpl-mock".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: params.model_id.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: Delta {
                        role: Some("assistant".to_string()),
                        content: None,
                    },
                    finish_reason: None,
                }],
                usage: None,
            },
            StreamChunk {
                id: "chatcmpl-mock".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: params.model_id.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: Delta {
                        role: None,
                        content: Some("Hello!".to_string()),
                    },
                    finish_reason: None,
                }],
                usage: None,
            },
            StreamChunk {
                id: "chatcmpl-mock".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: params.model_id.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: Delta {
                        role: None,
                        content: Some(" This is".to_string()),
                    },
                    finish_reason: None,
                }],
                usage: None,
            },
            StreamChunk {
                id: "chatcmpl-mock".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: params.model_id.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: Delta {
                        role: None,
                        content: Some(" a mock response.".to_string()),
                    },
                    finish_reason: None,
                }],
                usage: None,
            },
            StreamChunk {
                id: "chatcmpl-mock".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: params.model_id,
                choices: vec![StreamChoice {
                    index: 0,
                    delta: Delta {
                        role: None,
                        content: None,
                    },
                    finish_reason: Some("stop".to_string()),
                }],
                usage: Some(TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    total_tokens: 30,
                }),
            },
        ];
        
        Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
    }

    async fn text_completion(&self, params: CompletionParams) -> Result<CompletionResult, CompletionError> {
        // Validate model
        if params.model_id.is_empty() {
            return Err(CompletionError::InvalidModel("Model ID cannot be empty".to_string()));
        }

        // Validate prompt
        if params.prompt.is_empty() {
            return Err(CompletionError::InvalidParams("Prompt cannot be empty".to_string()));
        }

        // Generate mock completion based on prompt
        let completion_text = match &params.prompt.to_lowercase() {
            prompt if prompt.contains("once upon a time") => " there was a brave knight who embarked on an epic adventure to save the kingdom from an ancient dragon.".to_string(),
            prompt if prompt.contains("the future of ai") => " will likely involve more sophisticated reasoning capabilities, better integration with human workflows, and enhanced safety measures.".to_string(),
            prompt if prompt.contains("explain") => " in simple terms: this is a concept that can be understood by breaking it down into smaller, more manageable parts.".to_string(),
            _ => format!(" continuation of your prompt: '{}'", params.prompt),
        };

        // Mock token calculation
        let prompt_tokens = params.prompt.split_whitespace().count() as u32;
        let completion_tokens = completion_text.split_whitespace().count() as u32;

        Ok(CompletionResult {
            text: completion_text,
            finish_reason: FinishReason::Stop,
            usage: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            },
        })
    }
    
    async fn text_completion_stream(&self, _params: CompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        Err(CompletionError::InternalError("Streaming not supported in mock service".to_string()))
    }
    
    async fn get_available_models(&self) -> Result<Vec<crate::providers::ModelInfo>, CompletionError> {
        // Return mock models for testing
        Ok(vec![
            crate::providers::ModelInfo {
                id: "mock-chat-model".to_string(),
                object: "model".to_string(),
                created: Some(1699000000),
                owned_by: Some("mock-provider".to_string()),
                provider: "mock".to_string(),
            },
            crate::providers::ModelInfo {
                id: "mock-completion-model".to_string(),
                object: "model".to_string(),
                created: Some(1699000000),
                owned_by: Some("mock-provider".to_string()),
                provider: "mock".to_string(),
            },
        ])
    }
    
    
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub struct MockTdxHandler;

#[async_trait]
impl TdxHandler for MockTdxHandler {
    async fn get_quote(&self) -> Result<QuoteResponse, CompletionError> {
        // Return mock quote for testing
        Ok(QuoteResponse {
            gateway: GatewayQuote {
                quote: "mock-quote-base64-encoded".to_string(),
                measurement: "MRENCLAVE:mock-measurement".to_string(),
                svn: 1,
                build: BuildInfo {
                    image: "ghcr.io/agenthub/gateway:mock".to_string(),
                    sbom: "sha256:mock-sbom-hash".to_string(),
                },
            },
            allowlist: vec![
                ServiceAllowlistEntry {
                    service: "mock-service".to_string(), 
                    expected_measurements: vec!["sha256:mock-measurement".to_string()],
                    min_svn: 1,
                    identifier: "ledger://compose/sha256:mock".to_string(),
                },
            ],
        })
    }
}
