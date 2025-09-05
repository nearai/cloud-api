use crate::models::*;

// ============================================================================
// HTTP to Domain Conversions
// ============================================================================

impl From<&crate::models::Message> for domain::ChatMessage {
    fn from(msg: &crate::models::Message) -> Self {
        Self {
            role: match msg.role.as_str() {
                "system" => domain::MessageRole::System,
                "user" => domain::MessageRole::User,
                "assistant" => domain::MessageRole::Assistant,
                _ => domain::MessageRole::User, // Default to user for unknown roles
            },
            content: msg.content.clone(),
            name: msg.name.clone(),
        }
    }
}

impl From<&ChatCompletionRequest> for domain::ChatCompletionParams {
    fn from(req: &ChatCompletionRequest) -> Self {
        Self {
            model_id: req.model.clone(),
            messages: req.messages.iter().map(|m| m.into()).collect(),
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            stop_sequences: req.stop.clone(),
            stream: req.stream,
        }
    }
}

impl From<&CompletionRequest> for domain::CompletionParams {
    fn from(req: &CompletionRequest) -> Self {
        Self {
            model_id: req.model.clone(),
            prompt: req.prompt.clone(),
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            stop_sequences: req.stop.clone(),
            stream: req.stream,
        }
    }
}

// ============================================================================
// Domain to HTTP Conversions
// ============================================================================

impl From<&domain::ChatMessage> for crate::models::Message {
    fn from(msg: &domain::ChatMessage) -> Self {
        Self {
            role: match msg.role {
                domain::MessageRole::System => "system".to_string(),
                domain::MessageRole::User => "user".to_string(),
                domain::MessageRole::Assistant => "assistant".to_string(),
            },
            content: msg.content.clone(),
            name: msg.name.clone(),
        }
    }
}

fn finish_reason_to_string(reason: &domain::FinishReason) -> String {
    match reason {
        domain::FinishReason::Stop => "stop".to_string(),
        domain::FinishReason::Length => "length".to_string(),
        domain::FinishReason::ContentFilter => "content_filter".to_string(),
    }
}

impl From<&domain::TokenUsage> for crate::models::Usage {
    fn from(usage: &domain::TokenUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }
    }
}

pub fn chat_completion_to_http_response(
    result: domain::ChatCompletionResult,
    request_model: &str, 
    id: String, 
    created: u64
) -> ChatCompletionResponse {
    ChatCompletionResponse {
        id,
        object: "chat.completion".to_string(),
        created,
        model: request_model.to_string(),
        choices: vec![ChatChoice {
            index: 0,
            message: (&result.message).into(),
            finish_reason: Some(finish_reason_to_string(&result.finish_reason)),
        }],
        usage: (&result.usage).into(),
    }
}

pub fn completion_to_http_response(
    result: domain::CompletionResult,
    request_model: &str,
    id: String,
    created: u64
) -> CompletionResponse {
    CompletionResponse {
        id,
        object: "text_completion".to_string(),
        created,
        model: request_model.to_string(),
        choices: vec![CompletionChoice {
            index: 0,
            text: result.text,
            logprobs: None,
            finish_reason: Some(finish_reason_to_string(&result.finish_reason)),
        }],
        usage: (&result.usage).into(),
    }
}

// ============================================================================
// Error Conversions
// ============================================================================

impl From<domain::CompletionError> for crate::models::ErrorResponse {
    fn from(err: domain::CompletionError) -> Self {
        match err {
            domain::CompletionError::InvalidModel(msg) => {
                ErrorResponse::with_param(msg, "invalid_request_error".to_string(), "model".to_string())
            }
            domain::CompletionError::InvalidParams(msg) => {
                ErrorResponse::new(msg, "invalid_request_error".to_string())
            }
            domain::CompletionError::RateLimited => {
                ErrorResponse::new("Rate limit exceeded".to_string(), "rate_limit_exceeded".to_string())
            }
            domain::CompletionError::InternalError(msg) => {
                ErrorResponse::new(format!("Internal server error: {}", msg), "internal_error".to_string())
            }
        }
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

pub fn generate_completion_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    
    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .hash(&mut hasher);
    
    format!("{:x}", hasher.finish())
}

pub fn current_unix_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_conversion() {
        let http_msg = crate::models::Message {
            role: "user".to_string(),
            content: "Hello".to_string(),
            name: None,
        };
        
        let domain_msg: domain::ChatMessage = (&http_msg).into();
        assert!(matches!(domain_msg.role, domain::MessageRole::User));
        assert_eq!(domain_msg.content, "Hello");
        
        let back_to_http: crate::models::Message = (&domain_msg).into();
        assert_eq!(back_to_http.role, "user");
        assert_eq!(back_to_http.content, "Hello");
    }

    #[test]
    fn test_chat_completion_request_conversion() {
        let http_req = ChatCompletionRequest {
            model: "gpt-3.5-turbo".to_string(),
            messages: vec![crate::models::Message {
                role: "user".to_string(),
                content: "Test message".to_string(),
                name: None,
            }],
            max_tokens: Some(100),
            temperature: Some(0.7),
            top_p: Some(1.0),
            n: Some(1),
            stream: None,
            stop: Some(vec!["\\n".to_string()]),
            presence_penalty: None,
            frequency_penalty: None,
        };
        
        let domain_params: domain::ChatCompletionParams = (&http_req).into();
        assert_eq!(domain_params.model_id, "gpt-3.5-turbo");
        assert_eq!(domain_params.messages.len(), 1);
        assert_eq!(domain_params.max_tokens, Some(100));
        assert_eq!(domain_params.temperature, Some(0.7));
        assert_eq!(domain_params.stop_sequences, Some(vec!["\\n".to_string()]));
    }
}
