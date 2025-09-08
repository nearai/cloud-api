use serde::{Deserialize, Serialize};

// Streaming response models
#[derive(Debug, Serialize, Deserialize)]
pub struct StreamChunkResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamChoice {
    pub index: u32,
    pub delta: Delta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: Option<u32>,
    #[serde(default = "default_temperature")]
    pub temperature: Option<f32>,
    #[serde(default = "default_top_p")]
    pub top_p: Option<f32>,
    #[serde(default = "default_n")]
    pub n: Option<u32>,
    pub stream: Option<bool>,
    pub stop: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String, // "system", "user", "assistant"
    pub content: String,
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String, // "chat.completion"
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize)]
pub struct ChatChoice {
    pub index: u32,
    pub message: Message,
    pub finish_reason: Option<String>, // "stop", "length", "content_filter"
}

#[derive(Debug, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    pub prompt: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: Option<u32>,
    #[serde(default = "default_temperature")]
    pub temperature: Option<f32>,
    #[serde(default = "default_top_p")]
    pub top_p: Option<f32>,
    #[serde(default = "default_n")]
    pub n: Option<u32>,
    pub stream: Option<bool>,
    pub logprobs: Option<u32>,
    pub echo: Option<bool>,
    pub stop: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub best_of: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct CompletionResponse {
    pub id: String,
    pub object: String, // "text_completion"
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub owned_by: String,
}

#[derive(Debug, Serialize)]
pub struct CompletionChoice {
    pub index: u32,
    pub text: String,
    pub logprobs: Option<serde_json::Value>,
    pub finish_reason: Option<String>, // "stop", "length", "content_filter"
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub message: String,
    pub r#type: String,
    pub param: Option<String>,
    pub code: Option<String>,
}

fn default_max_tokens() -> Option<u32> {
    Some(100)
}

fn default_temperature() -> Option<f32> {
    Some(1.0)
}

fn default_top_p() -> Option<f32> {
    Some(1.0)
}

fn default_n() -> Option<u32> {
    Some(1)
}

impl ChatCompletionRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.model.is_empty() {
            return Err("model is required".to_string());
        }
        
        if self.messages.is_empty() {
            return Err("messages cannot be empty".to_string());
        }
        
        for message in &self.messages {
            if message.role.is_empty() {
                return Err("message role is required".to_string());
            }
            if !["system", "user", "assistant"].contains(&message.role.as_str()) {
                return Err(format!("invalid message role: {}", message.role));
            }
        }
        
        if let Some(temp) = self.temperature {
            if !(0.0..=2.0).contains(&temp) {
                return Err("temperature must be between 0 and 2".to_string());
            }
        }
        
        if let Some(top_p) = self.top_p {
            if !(0.0..=1.0).contains(&top_p) {
                return Err("top_p must be between 0 and 1".to_string());
            }
        }
        
        Ok(())
    }
}

impl CompletionRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.model.is_empty() {
            return Err("model is required".to_string());
        }
        
        if self.prompt.is_empty() {
            return Err("prompt is required".to_string());
        }
        
        if let Some(temp) = self.temperature {
            if !(0.0..=2.0).contains(&temp) {
                return Err("temperature must be between 0 and 2".to_string());
            }
        }
        
        if let Some(top_p) = self.top_p {
            if !(0.0..=1.0).contains(&top_p) {
                return Err("top_p must be between 0 and 1".to_string());
            }
        }
        
        Ok(())
    }
}

impl Usage {
    pub fn new(prompt_tokens: u32, completion_tokens: u32) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        }
    }
}

impl ErrorResponse {
    pub fn new(message: String, error_type: String) -> Self {
        Self {
            error: ErrorDetail {
                message,
                r#type: error_type,
                param: None,
                code: None,
            },
        }
    }
    
    pub fn with_param(message: String, error_type: String, param: String) -> Self {
        Self {
            error: ErrorDetail {
                message,
                r#type: error_type,
                param: Some(param),
                code: None,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct QuoteResponse {
    pub gateway: GatewayQuote,
    pub allowlist: Vec<ServiceAllowlistEntry>,
}

#[derive(Debug, Serialize)]
pub struct GatewayQuote {
    pub quote: String,
    pub measurement: String,
    pub svn: u32,
    pub build: BuildInfo,
}

#[derive(Debug, Serialize)]
pub struct ServiceAllowlistEntry {
    pub service: String,
    pub expected_measurements: Vec<String>,
    pub min_svn: u32,
    pub identifier: String,
}

#[derive(Debug, Serialize)]
pub struct BuildInfo {
    pub image: String,
    pub sbom: String,
}
