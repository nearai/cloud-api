use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use crate::{
    errors::CompletionError,
    models::*,
    providers::{StreamChunk, ModelInfo, Delta, StreamChoice},
    services::CompletionHandler,
};

/// Mock provider for testing provider-level functionality
pub struct MockProvider {
    name: String,
    models: Vec<String>,
}

impl MockProvider {
    pub fn new(name: String) -> Self {
        Self {
            name,
            models: vec![
                "mock-model-small".to_string(),
                "mock-model-large".to_string(),
            ],
        }
    }
    
    pub fn with_models(name: String, models: Vec<String>) -> Self {
        Self { name, models }
    }
}

#[async_trait]
impl CompletionHandler for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }
    
    async fn get_available_models(&self) -> Result<Vec<ModelInfo>, CompletionError> {
        Ok(self.models.iter().map(|id| ModelInfo {
            id: id.clone(),
            object: "model".to_string(),
            created: Some(1234567890),
            owned_by: Some("mock".to_string()),
            provider: self.name.clone(),
        }).collect())
    }
    
    fn supports_model(&self, model_id: &str) -> bool {
        self.models.iter().any(|m| m == model_id)
    }
    
    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResult, CompletionError> {
        // Simple mock response based on input
        let last_message = params.messages.last()
            .ok_or_else(|| CompletionError::InvalidParams("No messages provided".to_string()))?;
        
        let response_content = format!("Mock response to: {}", 
            last_message.content.as_ref().unwrap_or(&"empty".to_string()));
        
        Ok(ChatCompletionResult {
            message: ChatMessage {
                role: MessageRole::Assistant,
                content: Some(response_content),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: FinishReason::Stop,
            usage: TokenUsage::new(10, 5),
        })
    }
    
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        // Simple mock streaming response
        let response = "This is a mock streaming response.";
        let model = params.model_id.clone();
        
        let stream = futures::stream::iter(vec![
            Ok(StreamChunk {
                id: "mock-stream-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: model.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: Delta {
                        role: Some("assistant".to_string()),
                        content: None,
                    },
                    finish_reason: None,
                }],
                usage: None,
            }),
            Ok(StreamChunk {
                id: "mock-stream-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model: model.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: Delta {
                        role: None,
                        content: Some(response.to_string()),
                    },
                    finish_reason: None,
                }],
                usage: None,
            }),
            Ok(StreamChunk {
                id: "mock-stream-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1234567890,
                model,
                choices: vec![StreamChoice {
                    index: 0,
                    delta: Delta {
                        role: None,
                        content: None,
                    },
                    finish_reason: Some("stop".to_string()),
                }],
                usage: Some(TokenUsage::new(10, 5)),
            }),
        ]);
        
        Ok(Box::pin(stream))
    }
    
    async fn text_completion(
        &self,
        params: CompletionParams,
    ) -> Result<CompletionResult, CompletionError> {
        Ok(CompletionResult {
            text: format!("Mock completion for prompt: {}", params.prompt),
            finish_reason: FinishReason::Stop,
            usage: TokenUsage::new(8, 4),
        })
    }
    
    async fn text_completion_stream(
        &self,
        _params: CompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        // Simplified mock - reuse chat streaming logic
        let stream = futures::stream::iter(vec![
            Ok(StreamChunk {
                id: "mock-text-stream-1".to_string(),
                object: "text_completion.chunk".to_string(),
                created: 1234567890,
                model: "mock-model".to_string(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: Delta {
                        role: None,
                        content: Some("Mock text stream".to_string()),
                    },
                    finish_reason: Some("stop".to_string()),
                }],
                usage: Some(TokenUsage::new(5, 3)),
            }),
        ]);
        
        Ok(Box::pin(stream))
    }
    
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_provider() {
        let provider = MockProvider::new("test-mock".to_string());
        
        // Test name
        assert_eq!(provider.name(), "test-mock");
        
        // Test model support
        assert!(provider.supports_model("mock-model-small"));
        assert!(!provider.supports_model("unknown-model"));
        
        // Test get_available_models  
        let models = provider.get_available_models().await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "mock-model-small");
    }
}
