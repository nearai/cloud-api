use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tracing::{debug, error};

use crate::{
    errors::CompletionError,
    models::*,
    providers::{CompletionProvider, StreamChunk, ModelInfo},
};

// ============================================================================
// vLLM Provider Implementation
// ============================================================================

pub struct VLlmProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    name: String,
    supported_models: std::sync::RwLock<Vec<String>>,  // Cache for discovered models
}

impl VLlmProvider {
    pub fn new(name: String, base_url: String, api_key: Option<String>) -> Self {
        Self {
            client: Client::new(),
            base_url,
            api_key,
            name,
            supported_models: std::sync::RwLock::new(Vec::new()),
        }
    }
}

#[async_trait]
impl CompletionProvider for VLlmProvider {
    fn name(&self) -> &str {
        &self.name
    }
    
    async fn get_models(&self) -> Result<Vec<ModelInfo>, CompletionError> {
        let url = format!("{}/v1/models", self.base_url);
        
        let mut request = self.client.get(&url);
        if let Some(api_key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }
        
        let response = request.send().await
            .map_err(|e| CompletionError::InternalError(format!("Failed to fetch models: {}", e)))?;
            
        if !response.status().is_success() {
            return Err(CompletionError::InternalError(
                format!("Models endpoint returned {}: {}", 
                    response.status(), 
                    response.text().await.unwrap_or_default())
            ));
        }
        
        let vllm_models_response: VLlmModelsResponse = response.json().await
            .map_err(|e| CompletionError::InternalError(format!("Failed to parse models response: {}", e)))?;
        
        // Update cached models
        {
            let mut cached_models = self.supported_models.write().unwrap();
            *cached_models = vllm_models_response.data.iter().map(|m| m.id.clone()).collect();
        }
        
        // Convert vLLM models to our internal format with provider info
        let models_with_provider = vllm_models_response.data.into_iter().map(|vllm_model| {
            ModelInfo {
                id: vllm_model.id,
                object: vllm_model.object,
                created: Some(vllm_model.created),
                owned_by: Some(vllm_model.owned_by.unwrap_or_else(|| "vllm".to_string())),
                provider: self.name.clone(),
            }
        }).collect();
        
        Ok(models_with_provider)
    }
    
    fn supports_model(&self, model_id: &str) -> bool {
        self.supported_models.read().unwrap().iter().any(|m| m == model_id)
    }
    
    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResult, CompletionError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        
        let request_body = VLlmChatRequest {
            model: params.model_id.clone(),
            messages: params.messages.clone(),
            max_tokens: params.max_tokens,
            temperature: params.temperature,
            top_p: params.top_p,
            stop: params.stop_sequences,
            stream: Some(false),
        };
        
        debug!("Sending chat completion request to vLLM: {:?}", request_body);
        
        let mut request = self.client.post(&url)
            .json(&request_body);
            
        if let Some(api_key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }
        
        let response = request.send().await
            .map_err(|e| CompletionError::InternalError(format!("Request failed: {}", e)))?;
            
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await
                .unwrap_or_else(|_| "Could not read error response".to_string());
            return Err(CompletionError::InternalError(
                format!("vLLM API error ({}): {}", status, text)
            ));
        }
        
        let vllm_response: VLlmChatResponse = response.json().await
            .map_err(|e| CompletionError::InternalError(format!("Failed to parse response: {}", e)))?;
            
        // Convert vLLM response to our domain model
        let choice = vllm_response.choices.into_iter().next()
            .ok_or_else(|| CompletionError::InternalError("No choices in response".to_string()))?;
            
        Ok(ChatCompletionResult {
            message: choice.message,
            finish_reason: parse_finish_reason(&choice.finish_reason),
            usage: vllm_response.usage,
        })
    }
    
    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        
        let request_body = VLlmChatRequest {
            model: params.model_id.clone(),
            messages: params.messages.clone(),
            max_tokens: params.max_tokens,
            temperature: params.temperature,
            top_p: params.top_p,
            stop: params.stop_sequences,
            stream: Some(true),
        };
        
        debug!("Sending streaming chat completion request to vLLM");
        
        let mut request = self.client.post(&url)
            .json(&request_body);
            
        if let Some(api_key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }
        
        let response = request.send().await
            .map_err(|e| CompletionError::InternalError(format!("Request failed: {}", e)))?;
            
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await
                .unwrap_or_else(|_| "Could not read error response".to_string());
            return Err(CompletionError::InternalError(
                format!("vLLM API error ({}): {}", status, text)
            ));
        }
        
        // Create a stream from the response
        let stream = response.bytes_stream()
            .filter_map(|result| async move {
                match result {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        // Parse SSE format
                        for line in text.lines() {
                            if line.starts_with("data: ") {
                                let json_str = &line[6..];
                                if json_str == "[DONE]" {
                                    return None;
                                }
                                
                                match serde_json::from_str::<StreamChunk>(json_str) {
                                    Ok(chunk) => return Some(Ok(chunk)),
                                    Err(e) => {
                                        error!("Failed to parse SSE chunk: {}", e);
                                        return Some(Err(CompletionError::InternalError(
                                            format!("Failed to parse stream chunk: {}", e)
                                        )));
                                    }
                                }
                            }
                        }
                        None
                    }
                    Err(e) => Some(Err(CompletionError::InternalError(
                        format!("Stream error: {}", e)
                    )))
                }
            });
        
        Ok(Box::pin(stream))
    }
    
    async fn text_completion(
        &self,
        params: CompletionParams,
    ) -> Result<CompletionResult, CompletionError> {
        let url = format!("{}/v1/completions", self.base_url);
        
        let request_body = VLlmCompletionRequest {
            model: params.model_id.clone(),
            prompt: params.prompt.clone(),
            max_tokens: params.max_tokens,
            temperature: params.temperature,
            top_p: params.top_p,
            stop: params.stop_sequences,
            stream: Some(false),
        };
        
        debug!("Sending text completion request to vLLM: {:?}", request_body);
        
        let mut request = self.client.post(&url)
            .json(&request_body);
            
        if let Some(api_key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }
        
        let response = request.send().await
            .map_err(|e| CompletionError::InternalError(format!("Request failed: {}", e)))?;
            
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await
                .unwrap_or_else(|_| "Could not read error response".to_string());
            return Err(CompletionError::InternalError(
                format!("vLLM API error ({}): {}", status, text)
            ));
        }
        
        let vllm_response: VLlmCompletionResponse = response.json().await
            .map_err(|e| CompletionError::InternalError(format!("Failed to parse response: {}", e)))?;
            
        // Convert vLLM response to our domain model
        let choice = vllm_response.choices.into_iter().next()
            .ok_or_else(|| CompletionError::InternalError("No choices in response".to_string()))?;
            
        Ok(CompletionResult {
            text: choice.text,
            finish_reason: parse_finish_reason(&choice.finish_reason),
            usage: vllm_response.usage,
        })
    }
    
    async fn text_completion_stream(
        &self,
        params: CompletionParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        let url = format!("{}/v1/completions", self.base_url);
        
        let request_body = VLlmCompletionRequest {
            model: params.model_id.clone(),
            prompt: params.prompt.clone(),
            max_tokens: params.max_tokens,
            temperature: params.temperature,
            top_p: params.top_p,
            stop: params.stop_sequences,
            stream: Some(true),
        };
        
        debug!("Sending streaming text completion request to vLLM");
        
        let mut request = self.client.post(&url)
            .json(&request_body);
            
        if let Some(api_key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }
        
        let response = request.send().await
            .map_err(|e| CompletionError::InternalError(format!("Request failed: {}", e)))?;
            
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await
                .unwrap_or_else(|_| "Could not read error response".to_string());
            return Err(CompletionError::InternalError(
                format!("vLLM API error ({}): {}", status, text)
            ));
        }
        
        // Create a stream from the response - text completions use same SSE format
        let stream = response.bytes_stream()
            .filter_map(|result| async move {
                match result {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        // Parse SSE format
                        for line in text.lines() {
                            if line.starts_with("data: ") {
                                let json_str = &line[6..];
                                if json_str == "[DONE]" {
                                    return None;
                                }
                                
                                match serde_json::from_str::<StreamChunk>(json_str) {
                                    Ok(chunk) => return Some(Ok(chunk)),
                                    Err(e) => {
                                        error!("Failed to parse SSE chunk: {}", e);
                                        return Some(Err(CompletionError::InternalError(
                                            format!("Failed to parse stream chunk: {}", e)
                                        )));
                                    }
                                }
                            }
                        }
                        None
                    }
                    Err(e) => Some(Err(CompletionError::InternalError(
                        format!("Stream error: {}", e)
                    )))
                }
            });
        
        Ok(Box::pin(stream))
    }
}

// ============================================================================
// vLLM API Models
// ============================================================================

#[derive(Debug, Serialize)]
struct VLlmChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Debug, Serialize)]
struct VLlmCompletionRequest {
    model: String,
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct VLlmChatResponse {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    object: String,
    #[allow(dead_code)]
    created: u64,
    #[allow(dead_code)]
    model: String,
    choices: Vec<VLlmChatChoice>,
    usage: TokenUsage,
}

#[derive(Debug, Deserialize)]
struct VLlmCompletionResponse {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    object: String,
    #[allow(dead_code)]
    created: u64,
    #[allow(dead_code)]
    model: String,
    choices: Vec<VLlmCompletionChoice>,
    usage: TokenUsage,
}

#[derive(Debug, Deserialize)]
struct VLlmChatChoice {
    #[allow(dead_code)]
    index: u32,
    message: ChatMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VLlmCompletionChoice {
    #[allow(dead_code)]
    index: u32,
    text: String,
    finish_reason: Option<String>,
}

// ============================================================================
// Helper Functions
// ============================================================================

// ============================================================================
// vLLM Models Response (different from our internal ModelsResponse)
// ============================================================================

#[derive(Debug, Deserialize)]
struct VLlmModelsResponse {
    #[allow(dead_code)]
    object: String,
    data: Vec<VLlmModelInfo>,
}

#[derive(Debug, Deserialize)]
struct VLlmModelInfo {
    id: String,
    object: String,
    created: u64,
    owned_by: Option<String>,
    // vLLM may have additional fields we don't need
}

fn parse_finish_reason(reason: &Option<String>) -> FinishReason {
    match reason.as_deref() {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vllm_provider_creation() {
        let provider = VLlmProvider::new(
            "test-provider".to_string(),
            "http://localhost:8000".to_string(),
            Some("test-api-key".to_string()),
        );
        
        assert_eq!(provider.name(), "test-provider");
        assert_eq!(provider.base_url, "http://localhost:8000");
        assert_eq!(provider.api_key, Some("test-api-key".to_string()));
    }
}
