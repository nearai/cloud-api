//! Gemini backend implementation
//!
//! This backend handles HTTP communication with Google's Gemini API.
//! Format conversion is handled by the `converter` module.

mod converter;

use super::backend::{BackendConfig, ExternalBackend};
use crate::{
    BufferedSSEParser, ChatCompletionParams, ChatCompletionResponse, ChatCompletionResponseChoice,
    ChatCompletionResponseWithBytes, ChatResponseMessage, CompletionError, ImageData,
    ImageGenerationError, ImageGenerationParams, ImageGenerationResponse,
    ImageGenerationResponseWithBytes, MessageRole, StreamingResult, TokenUsage,
};
use async_trait::async_trait;
use bytes::Bytes;
use converter::{
    convert_messages, convert_tools, extract_response_content, map_finish_reason_string,
    GeminiEventParser, GeminiGenerationConfig, GeminiParserState, GeminiRequest, GeminiResponse,
};
use futures_util::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Gemini backend - handles HTTP communication with Google's Gemini API
pub struct GeminiBackend {
    client: Client,
}

impl GeminiBackend {
    pub fn new() -> Self {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .expect("Failed to create HTTP client");

        Self { client }
    }

    fn build_headers(
        &self,
        config: &BackendConfig,
    ) -> Result<reqwest::header::HeaderMap, CompletionError> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Content-Type",
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            "x-goog-api-key",
            reqwest::header::HeaderValue::from_str(&config.api_key)
                .map_err(|e| CompletionError::CompletionError(format!("Invalid API key: {e}")))?,
        );
        Ok(headers)
    }

    fn build_request(&self, params: &ChatCompletionParams) -> GeminiRequest {
        let (system_instruction, contents) = convert_messages(&params.messages);
        let max_tokens = params.max_completion_tokens.or(params.max_tokens);

        let generation_config = if params.temperature.is_some()
            || params.top_p.is_some()
            || max_tokens.is_some()
            || params.stop.is_some()
        {
            Some(GeminiGenerationConfig {
                temperature: params.temperature,
                top_p: params.top_p,
                max_output_tokens: max_tokens,
                stop_sequences: params.stop.clone(),
            })
        } else {
            None
        };

        // Convert tools if provided
        let tools = params.tools.as_ref().map(|t| convert_tools(t));
        let tools = tools.filter(|t| !t.is_empty());

        GeminiRequest {
            contents,
            system_instruction,
            generation_config,
            tools,
        }
    }
}

impl Default for GeminiBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Strip vendor prefix from model name (e.g., "google/gemini-2.0-flash" -> "gemini-2.0-flash")
fn strip_vendor_prefix(model: &str) -> &str {
    if let Some(stripped) = model.strip_prefix("google/") {
        return stripped;
    }
    if let Some(stripped) = model.strip_prefix("vertex/") {
        return stripped;
    }
    model
}

/// SSE parser type alias for Gemini
pub type GeminiSSEParser<S> = BufferedSSEParser<S, GeminiEventParser>;

/// Create a new Gemini SSE parser
pub fn new_gemini_sse_parser<S>(stream: S, model: String) -> GeminiSSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    BufferedSSEParser::new(stream, GeminiParserState::new(model))
}

// ==================== Image Generation Structures ====================

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiImageGenerationRequest {
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    number_of_images: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aspect_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    safety_filter_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    person_generation: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiImageGenerationResponse {
    #[serde(default)]
    generated_images: Vec<GeminiGeneratedImage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGeneratedImage {
    image: GeminiImageData,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiImageData {
    image_bytes: String,
}

fn size_to_aspect_ratio(size: Option<&String>) -> Option<String> {
    size.and_then(|s| {
        let parts: Vec<&str> = s.split('x').collect();
        if parts.len() == 2 {
            if let (Ok(w), Ok(h)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                let ratio = w as f32 / h as f32;
                if (ratio - 1.0).abs() < 0.1 {
                    return Some("1:1".to_string());
                } else if (ratio - 16.0 / 9.0).abs() < 0.1 {
                    return Some("16:9".to_string());
                } else if (ratio - 9.0 / 16.0).abs() < 0.1 {
                    return Some("9:16".to_string());
                } else if (ratio - 4.0 / 3.0).abs() < 0.1 {
                    return Some("4:3".to_string());
                } else if (ratio - 3.0 / 4.0).abs() < 0.1 {
                    return Some("3:4".to_string());
                }
            }
        }
        None
    })
}

#[async_trait]
impl ExternalBackend for GeminiBackend {
    fn backend_type(&self) -> &'static str {
        "gemini"
    }

    async fn chat_completion_stream(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        let model_name = strip_vendor_prefix(model);
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            config.base_url, model_name
        );

        let request = self.build_request(&params);

        let headers = self.build_headers(config)?;
        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
            return Err(CompletionError::HttpError {
                status_code,
                message: error_text,
            });
        }

        let sse_stream = new_gemini_sse_parser(response.bytes_stream(), model.to_string());
        Ok(Box::pin(sse_stream))
    }

    async fn chat_completion(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        let model_name = strip_vendor_prefix(model);
        let url = format!("{}/models/{}:generateContent", config.base_url, model_name);

        let request = self.build_request(&params);
        let headers = self.build_headers(config)?;
        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("Failed to read error response body: {e}"));
            return Err(CompletionError::HttpError {
                status_code,
                message: error_text,
            });
        }

        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e| CompletionError::CompletionError(e.to_string()))?
            .to_vec();

        let gemini_response: GeminiResponse = serde_json::from_slice(&raw_bytes).map_err(|e| {
            CompletionError::CompletionError(format!("Failed to parse response: {e}"))
        })?;

        if gemini_response.candidates.is_empty() {
            return Err(CompletionError::CompletionError(
                "No candidates in Gemini response".to_string(),
            ));
        }

        let candidate = &gemini_response.candidates[0];
        let (content, tool_calls) = extract_response_content(&candidate.content.parts);

        // Determine finish reason - tool_calls if we have function calls
        let finish_reason = if tool_calls.is_some() {
            Some("tool_calls".to_string())
        } else {
            map_finish_reason_string(candidate.finish_reason.as_ref())
        };

        let openai_response = ChatCompletionResponse {
            id: format!("gemini-{}", Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatResponseMessage {
                    role: MessageRole::Assistant,
                    content,
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls,
                    reasoning_content: None,
                    reasoning: None,
                },
                logprobs: None,
                finish_reason,
                token_ids: None,
            }],
            service_tier: None,
            system_fingerprint: None,
            usage: TokenUsage {
                prompt_tokens: gemini_response.usage_metadata.prompt_token_count,
                completion_tokens: gemini_response.usage_metadata.candidates_token_count,
                total_tokens: gemini_response.usage_metadata.total_token_count,
                prompt_tokens_details: None,
            },
            prompt_logprobs: None,
            prompt_token_ids: None,
            kv_transfer_params: None,
        };

        let serialized_bytes = serde_json::to_vec(&openai_response).map_err(|e| {
            CompletionError::CompletionError(format!("Failed to serialize response: {e}"))
        })?;

        Ok(ChatCompletionResponseWithBytes {
            response: openai_response,
            raw_bytes: serialized_bytes,
        })
    }

    async fn image_generation(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ImageGenerationParams,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        let model_name = strip_vendor_prefix(model);
        let url = format!("{}/models/{}:generateImages", config.base_url, model_name);

        let gemini_request = GeminiImageGenerationRequest {
            prompt: params.prompt,
            number_of_images: params.n,
            aspect_ratio: size_to_aspect_ratio(params.size.as_ref()),
            safety_filter_level: None,
            person_generation: None,
        };

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Content-Type",
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            "x-goog-api-key",
            reqwest::header::HeaderValue::from_str(&config.api_key).map_err(|e| {
                ImageGenerationError::GenerationError(format!("Invalid API key: {e}"))
            })?,
        );

        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(timeout)
            .json(&gemini_request)
            .send()
            .await
            .map_err(|e| ImageGenerationError::GenerationError(e.to_string()))?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ImageGenerationError::HttpError {
                status_code,
                message,
            });
        }

        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e| ImageGenerationError::GenerationError(e.to_string()))?
            .to_vec();

        let gemini_response: GeminiImageGenerationResponse = serde_json::from_slice(&raw_bytes)
            .map_err(|e| {
                ImageGenerationError::GenerationError(format!("Failed to parse response: {e}"))
            })?;

        let openai_response = ImageGenerationResponse {
            id: format!("gemini-img-{}", Uuid::new_v4()),
            created: chrono::Utc::now().timestamp(),
            data: gemini_response
                .generated_images
                .into_iter()
                .map(|img| ImageData {
                    b64_json: Some(img.image.image_bytes),
                    url: None,
                    revised_prompt: None,
                })
                .collect(),
        };

        let serialized_bytes = serde_json::to_vec(&openai_response).map_err(|e| {
            ImageGenerationError::GenerationError(format!("Failed to serialize response: {e}"))
        })?;

        Ok(ImageGenerationResponseWithBytes {
            response: openai_response,
            raw_bytes: serialized_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_vendor_prefix_google() {
        assert_eq!(
            strip_vendor_prefix("google/gemini-2.0-flash"),
            "gemini-2.0-flash"
        );
    }

    #[test]
    fn test_strip_vendor_prefix_no_prefix() {
        assert_eq!(strip_vendor_prefix("gemini-2.0-flash"), "gemini-2.0-flash");
    }

    #[test]
    fn test_size_to_aspect_ratio_square() {
        assert_eq!(
            size_to_aspect_ratio(Some(&"1024x1024".to_string())),
            Some("1:1".to_string())
        );
    }

    #[test]
    fn test_size_to_aspect_ratio_none() {
        assert_eq!(size_to_aspect_ratio(None), None);
    }

    #[tokio::test]
    async fn test_sse_parser_multiple_events() {
        use futures_util::StreamExt;

        let packet = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1,\"totalTokenCount\":11}}\n\n";

        let bytes = bytes::Bytes::from(packet);
        let mock_stream = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes)]);

        let parser = new_gemini_sse_parser(mock_stream, "gemini-1.5-pro".to_string());
        let events: Vec<_> = parser.collect().await;

        assert_eq!(events.len(), 1);
        assert!(events[0].is_ok());
    }
}
