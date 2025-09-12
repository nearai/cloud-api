use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tracing::{debug, error};

use crate::{
    errors::CompletionError,
    models::*,
    providers::{StreamChunk, StreamChoice, Delta, ModelInfo},
    services::CompletionHandler,
};

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
impl CompletionHandler for VLlmProvider {
    fn name(&self) -> &str {
        &self.name
    }
    
    fn supports_model(&self, model_id: &str) -> bool {
        let models = self.supported_models.read().unwrap();
        models.iter().any(|m| m == model_id)
    }
    
    async fn get_available_models(&self) -> Result<Vec<ModelInfo>, CompletionError> {
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
            .map(|result| {
                match result {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        let mut chunks = Vec::new();
                        
                        // Parse SSE format - collect all chunks from this batch
                        for line in text.lines() {
                            if line.starts_with("data: ") {
                                let json_str = &line[6..];
                                if json_str == "[DONE]" {
                                    continue;
                                }
                                
                                // Parse vLLM stream chunk and convert to our format
                                match serde_json::from_str::<VLlmStreamChunk>(json_str) {
                                    Ok(vllm_chunk) => {
                                        // Convert vLLM format to our internal StreamChunk format
                                        let chunk = StreamChunk {
                                            id: vllm_chunk.id,
                                            object: vllm_chunk.object,
                                            created: vllm_chunk.created,
                                            model: vllm_chunk.model,
                                            choices: vllm_chunk.choices.into_iter().map(|c| {
                                                // For chat completions, use delta field; for text completions, use text field
                                                let (role, content) = if let Some(delta) = c.delta {
                                                    (delta.role, delta.content)
                                                } else if let Some(text) = c.text {
                                                    // Text completion format
                                                    (None, Some(text))
                                                } else {
                                                    (None, None)
                                                };
                                                
                                                StreamChoice {
                                                    index: c.index,
                                                    delta: Delta { role, content },
                                                    finish_reason: c.finish_reason,
                                                }
                                            }).collect(),
                                            usage: vllm_chunk.usage,
                                        };
                                        chunks.push(Ok(chunk));
                                    }
                                    Err(e) => {
                                        error!("Failed to parse SSE chunk: {}. Raw data: {}", e, json_str);
                                        // Try to parse as a generic JSON value to see the structure
                                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) {
                                            error!("Actual JSON structure: {}", serde_json::to_string_pretty(&value).unwrap_or_default());
                                        }
                                        chunks.push(Err(CompletionError::InternalError(
                                            format!("Failed to parse stream chunk: {}", e)
                                        )));
                                    }
                                }
                            }
                        }
                        chunks
                    }
                    Err(e) => vec![Err(CompletionError::InternalError(
                        format!("Stream error: {}", e)
                    ))]
                }
            })
            .flat_map(futures::stream::iter);
        
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
            .map(|result| {
                match result {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        let mut chunks = Vec::new();
                        
                        // Parse SSE format - collect all chunks from this batch
                        for line in text.lines() {
                            if line.starts_with("data: ") {
                                let json_str = &line[6..];
                                if json_str == "[DONE]" {
                                    continue;
                                }
                                
                                // Parse vLLM stream chunk and convert to our format
                                match serde_json::from_str::<VLlmStreamChunk>(json_str) {
                                    Ok(vllm_chunk) => {
                                        // Convert vLLM format to our internal StreamChunk format
                                        let chunk = StreamChunk {
                                            id: vllm_chunk.id,
                                            object: vllm_chunk.object,
                                            created: vllm_chunk.created,
                                            model: vllm_chunk.model,
                                            choices: vllm_chunk.choices.into_iter().map(|c| {
                                                // For chat completions, use delta field; for text completions, use text field
                                                let (role, content) = if let Some(delta) = c.delta {
                                                    (delta.role, delta.content)
                                                } else if let Some(text) = c.text {
                                                    // Text completion format
                                                    (None, Some(text))
                                                } else {
                                                    (None, None)
                                                };
                                                
                                                StreamChoice {
                                                    index: c.index,
                                                    delta: Delta { role, content },
                                                    finish_reason: c.finish_reason,
                                                }
                                            }).collect(),
                                            usage: vllm_chunk.usage,
                                        };
                                        chunks.push(Ok(chunk));
                                    }
                                    Err(e) => {
                                        error!("Failed to parse SSE chunk: {}. Raw data: {}", e, json_str);
                                        // Try to parse as a generic JSON value to see the structure
                                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) {
                                            error!("Actual JSON structure: {}", serde_json::to_string_pretty(&value).unwrap_or_default());
                                        }
                                        chunks.push(Err(CompletionError::InternalError(
                                            format!("Failed to parse stream chunk: {}", e)
                                        )));
                                    }
                                }
                            }
                        }
                        chunks
                    }
                    Err(e) => vec![Err(CompletionError::InternalError(
                        format!("Stream error: {}", e)
                    ))]
                }
            })
            .flat_map(futures::stream::iter);
        
        Ok(Box::pin(stream))
    }
    
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

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

// Streaming response structures for vLLM - these match the actual vLLM/OpenAI format
#[derive(Debug, Deserialize)]
struct VLlmStreamChunk {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<VLlmStreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<TokenUsage>,
}

#[derive(Debug, Deserialize)]
struct VLlmStreamChoice {
    index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<VLlmDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,  // For text completions
    #[serde(skip_serializing_if = "Option::is_none")]
    finish_reason: Option<String>,
    #[allow(dead_code)]  // vLLM sends this field but we don't use it yet
    #[serde(skip_serializing_if = "Option::is_none")]
    logprobs: Option<serde_json::Value>, // Might have logprobs, ignore for now
}

#[derive(Debug, Deserialize)]
struct VLlmDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
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
    use httpmock::prelude::*;
    use tokio_stream::StreamExt;
    use std::time::Duration;

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

    #[tokio::test]
    async fn test_chat_completion_stream_success() {
        // Start a mock server
        let server = MockServer::start();

        // Define SSE response chunks
        let sse_response = vec![
            "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}",
            "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}",
            "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"!\"},\"finish_reason\":null}]}",
            "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}",
            "data: [DONE]",
        ];

        // Create mock endpoint
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse_response.join("\n\n"));
        });

        // Create provider with mock server URL
        let provider = VLlmProvider::new(
            "test-provider".to_string(),
            server.url(""),
            None,
        );

        // Create chat completion parameters
        let params = ChatCompletionParams {
            model_id: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("Hello".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: Some(true),
            tools: None,
        };

        // Call the streaming method
        let stream = provider.chat_completion_stream(params).await.unwrap();
        let mut stream = Box::pin(stream);

        // Collect all chunks
        let mut chunks = Vec::new();
        let mut content = String::new();
        
        while let Some(result) = stream.next().await {
            match result {
                Ok(chunk) => {
                    // Verify chunk structure
                    assert_eq!(chunk.id, "chat-1");
                    assert_eq!(chunk.object, "chat.completion.chunk");
                    assert_eq!(chunk.model, "test-model");
                    
                    if let Some(choice) = chunk.choices.first() {
                        if let Some(text) = &choice.delta.content {
                            content.push_str(text);
                        }
                    }
                    
                    chunks.push(chunk);
                }
                Err(e) => panic!("Stream error: {:?}", e),
            }
        }

        // Verify we received all chunks
        assert_eq!(chunks.len(), 4, "Should have received 4 chunks (excluding [DONE])");
        assert_eq!(content, "Hello world!", "Accumulated content should match");
        
        // Verify the last chunk has finish_reason
        let last_chunk = &chunks[3];
        assert_eq!(last_chunk.choices[0].finish_reason, Some("stop".to_string()));

        // Verify mock was called
        mock.assert();
    }

    #[tokio::test]
    async fn test_text_completion_stream_success() {
        // Start a mock server
        let server = MockServer::start();

        // Define SSE response chunks for text completion
        let sse_response = vec![
            "data: {\"id\":\"cmpl-1\",\"object\":\"text_completion\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"The quick\"},\"finish_reason\":null}]}",
            "data: {\"id\":\"cmpl-1\",\"object\":\"text_completion\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" brown fox\"},\"finish_reason\":null}]}",
            "data: {\"id\":\"cmpl-1\",\"object\":\"text_completion\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" jumps\"},\"finish_reason\":null}]}",
            "data: {\"id\":\"cmpl-1\",\"object\":\"text_completion\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}",
            "data: [DONE]",
        ];

        // Create mock endpoint
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse_response.join("\n\n"));
        });

        // Create provider with mock server URL
        let provider = VLlmProvider::new(
            "test-provider".to_string(),
            server.url(""),
            None,
        );

        // Create completion parameters
        let params = CompletionParams {
            model_id: "test-model".to_string(),
            prompt: "Complete this:".to_string(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: Some(true),
        };

        // Call the streaming method
        let stream = provider.text_completion_stream(params).await.unwrap();
        let mut stream = Box::pin(stream);

        // Collect all chunks
        let mut chunks = Vec::new();
        let mut content = String::new();
        
        while let Some(result) = stream.next().await {
            match result {
                Ok(chunk) => {
                    // Verify chunk structure
                    assert_eq!(chunk.id, "cmpl-1");
                    assert_eq!(chunk.object, "text_completion");
                    assert_eq!(chunk.model, "test-model");
                    
                    if let Some(choice) = chunk.choices.first() {
                        if let Some(text) = &choice.delta.content {
                            content.push_str(text);
                        }
                    }
                    
                    chunks.push(chunk);
                }
                Err(e) => panic!("Stream error: {:?}", e),
            }
        }

        // Verify we received all chunks
        assert_eq!(chunks.len(), 4, "Should have received 4 chunks (excluding [DONE])");
        assert_eq!(content, "The quick brown fox jumps", "Accumulated content should match");
        
        // Verify the last chunk has finish_reason
        let last_chunk = &chunks[3];
        assert_eq!(last_chunk.choices[0].finish_reason, Some("length".to_string()));

        // Verify mock was called
        mock.assert();
    }

    #[tokio::test]
    async fn test_stream_with_multiple_lines_in_chunk() {
        // Test SSE parsing when multiple data lines come in a single chunk
        let server = MockServer::start();

        // Multiple SSE lines in one response
        let sse_response = concat!(
            "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"First\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" Second\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]"
        );

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse_response);
        });

        let provider = VLlmProvider::new(
            "test-provider".to_string(),
            server.url(""),
            None,
        );

        let params = ChatCompletionParams {
            model_id: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("Test".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: Some(true),
            tools: None,
        };

        let stream = provider.chat_completion_stream(params).await.unwrap();
        let mut stream = Box::pin(stream);

        let mut content = String::new();
        while let Some(result) = stream.next().await {
            if let Ok(chunk) = result {
                if let Some(choice) = chunk.choices.first() {
                    if let Some(text) = &choice.delta.content {
                        content.push_str(text);
                    }
                }
            }
        }

        assert_eq!(content, "First Second", "Should parse multiple SSE lines correctly");
        mock.assert();
    }

    #[tokio::test]
    async fn test_stream_error_response() {
        let server = MockServer::start();

        // Mock error response
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions");
            then.status(500)
                .header("content-type", "text/plain")
                .body("Internal server error");
        });

        let provider = VLlmProvider::new(
            "test-provider".to_string(),
            server.url(""),
            None,
        );

        let params = ChatCompletionParams {
            model_id: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("Test".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: Some(true),
            tools: None,
        };

        let result = provider.chat_completion_stream(params).await;
        
        assert!(result.is_err(), "Should return error for HTTP 500");
        if let Err(e) = result {
            match e {
                CompletionError::InternalError(msg) => {
                    assert!(msg.contains("vLLM API error"), "Error message should indicate API error");
                    assert!(msg.contains("500"), "Error message should contain status code");
                }
                _ => panic!("Expected InternalError variant"),
            }
        }

        mock.assert();
    }

    #[tokio::test]
    async fn test_stream_with_api_key() {
        let server = MockServer::start();

        // Mock endpoint that verifies API key
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("Authorization", "Bearer test-api-key");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body("data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Authenticated\"},\"finish_reason\":null}]}\n\ndata: [DONE]");
        });

        let provider = VLlmProvider::new(
            "test-provider".to_string(),
            server.url(""),
            Some("test-api-key".to_string()),
        );

        let params = ChatCompletionParams {
            model_id: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("Test".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: Some(true),
            tools: None,
        };

        let stream = provider.chat_completion_stream(params).await.unwrap();
        let mut stream = Box::pin(stream);

        let mut has_chunks = false;
        while let Some(result) = stream.next().await {
            if let Ok(_) = result {
                has_chunks = true;
            }
        }

        assert!(has_chunks, "Should receive at least one chunk");
        mock.assert();
    }

    #[tokio::test]
    async fn test_stream_malformed_sse() {
        let server = MockServer::start();

        // Malformed SSE response
        let sse_response = concat!(
            "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Valid\"},\"finish_reason\":null}]}\n\n",
            "data: {malformed json}\n\n",  // This will cause parse error
            "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" after error\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]"
        );

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse_response);
        });

        let provider = VLlmProvider::new(
            "test-provider".to_string(),
            server.url(""),
            None,
        );

        let params = ChatCompletionParams {
            model_id: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("Test".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: Some(true),
            tools: None,
        };

        let stream = provider.chat_completion_stream(params).await.unwrap();
        let mut stream = Box::pin(stream);

        let mut valid_chunks = 0;
        let mut errors = 0;
        
        while let Some(result) = stream.next().await {
            match result {
                Ok(_) => valid_chunks += 1,
                Err(e) => {
                    errors += 1;
                    match e {
                        CompletionError::InternalError(msg) => {
                            assert!(msg.contains("Failed to parse stream chunk"), "Error should indicate parsing failure");
                        }
                        _ => panic!("Expected InternalError for parse failure"),
                    }
                }
            }
        }

        assert_eq!(valid_chunks, 2, "Should have received 2 valid chunks");
        assert_eq!(errors, 1, "Should have received 1 parsing error");
        
        mock.assert();
    }

    #[tokio::test]
    async fn test_stream_timeout_handling() {
        let server = MockServer::start();

        // Mock a slow response
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .delay(Duration::from_millis(100))  // Add small delay
                .body("data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Delayed\"},\"finish_reason\":null}]}\n\ndata: [DONE]");
        });

        let provider = VLlmProvider::new(
            "test-provider".to_string(),
            server.url(""),
            None,
        );

        let params = ChatCompletionParams {
            model_id: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("Test".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: Some(true),
            tools: None,
        };

        // Should still work with delay
        let stream = provider.chat_completion_stream(params).await.unwrap();
        let mut stream = Box::pin(stream);

        let mut chunks = Vec::new();
        while let Some(result) = stream.next().await {
            if let Ok(chunk) = result {
                chunks.push(chunk);
            }
        }

        assert!(!chunks.is_empty(), "Should receive chunks even with delay");
        mock.assert();
    }
}
