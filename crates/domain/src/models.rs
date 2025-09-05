use serde::{Deserialize, Serialize};

// ============================================================================
// Domain Models - Business Logic Focused
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionParams {
    pub model_id: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Option<Vec<String>>,
    pub stream: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionParams {
    pub model_id: String,
    pub prompt: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Option<Vec<String>>,
    pub stream: Option<bool>,
}

#[derive(Debug)]
pub struct ChatCompletionResult {
    pub message: ChatMessage,
    pub finish_reason: FinishReason,
    pub usage: TokenUsage,
}

#[derive(Debug)]
pub struct CompletionResult {
    pub text: String,
    pub finish_reason: FinishReason,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone)]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl TokenUsage {
    pub fn new(prompt_tokens: u32, completion_tokens: u32) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_usage() {
        let usage = TokenUsage::new(10, 5);
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn test_message_creation() {
        let message = ChatMessage {
            role: MessageRole::User,
            content: "Hello, world!".to_string(),
            name: Some("test_user".to_string()),
        };
        
        assert!(matches!(message.role, MessageRole::User));
        assert_eq!(message.content, "Hello, world!");
        assert_eq!(message.name, Some("test_user".to_string()));
    }

    #[test]
    fn test_completion_params() {
        let params = ChatCompletionParams {
            model_id: "gpt-3.5-turbo".to_string(),
            messages: vec![
                ChatMessage {
                    role: MessageRole::System,
                    content: "You are a helpful assistant".to_string(),
                    name: None,
                },
                ChatMessage {
                    role: MessageRole::User,
                    content: "Hello!".to_string(),
                    name: None,
                }
            ],
            max_tokens: Some(100),
            temperature: Some(0.7),
            top_p: Some(1.0),
            stop_sequences: Some(vec!["\n".to_string()]),
            stream: None,
        };
        
        assert_eq!(params.model_id, "gpt-3.5-turbo");
        assert_eq!(params.messages.len(), 2);
        assert_eq!(params.max_tokens, Some(100));
        assert_eq!(params.temperature, Some(0.7));
    }
}
