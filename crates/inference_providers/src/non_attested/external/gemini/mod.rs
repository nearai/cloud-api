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
    ImageGenerationResponseWithBytes, MessageRole, StreamingResult,
};
use async_trait::async_trait;
use bytes::Bytes;
use converter::{
    convert_messages, convert_tools, extract_response_content, map_finish_reason_string,
    response_format_to_gemini, GeminiEventParser, GeminiGenerationConfig, GeminiParserState,
    GeminiPart, GeminiRequest, GeminiResponse,
};
use futures_util::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Convert a parsed Gemini `generateContent` response into the OpenAI-shaped
/// response we expose to clients.
///
/// Centralised so the empty/missing-content handling has a single source of
/// truth and can be exercised end-to-end in unit tests without an HTTP layer.
/// Emits a `tracing::warn!` when a candidate has no usable content (parts
/// empty or `content` missing) and the finish reason is *not* `MAX_TOKENS` —
/// that combination is the silent-regression case worth flagging: under the
/// old strict schema it would have surfaced as a 502, and we want Datadog to
/// catch it if Google ships a real upstream regression that produces empty
/// responses with `STOP`/`SAFETY`/etc.
fn convert_to_openai_response(
    gemini_response: GeminiResponse,
    model: &str,
) -> Result<ChatCompletionResponse, CompletionError> {
    if gemini_response.candidates.is_empty() {
        return Err(CompletionError::CompletionError(
            "No candidates in Gemini response".to_string(),
        ));
    }

    let candidate = &gemini_response.candidates[0];
    let parts: &[GeminiPart] = candidate
        .content
        .as_ref()
        .map_or(&[], |c| c.parts.as_slice());
    let (content, tool_calls) = extract_response_content(parts);

    if content.is_none() && tool_calls.is_none() {
        let fr = candidate.finish_reason.as_deref().unwrap_or("");
        if fr != "MAX_TOKENS" {
            tracing::warn!(
                model = %model,
                finish_reason = %fr,
                "Gemini returned a candidate with no usable content and finish_reason != MAX_TOKENS"
            );
        }
    }

    // Determine finish reason - tool_calls if we have function calls
    let finish_reason = if tool_calls.is_some() {
        Some("tool_calls".to_string())
    } else {
        map_finish_reason_string(candidate.finish_reason.as_ref())
    };

    Ok(ChatCompletionResponse {
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
            extra: Default::default(),
        }],
        service_tier: None,
        system_fingerprint: None,
        usage: gemini_response
            .usage_metadata
            .unwrap_or_default()
            .to_token_usage(),
        prompt_logprobs: None,
        prompt_token_ids: None,
        kv_transfer_params: None,
        extra: Default::default(),
    })
}

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

        // `seed` (nearai/cloud-api #669): Gemini supports deterministic sampling
        // via `generationConfig.seed`. The service layer hardcodes the typed
        // `seed` field to None and forwards the original in `extra`, so read
        // both (typed first, then the passthrough map).
        let seed = params
            .seed
            .or_else(|| params.extra.get("seed").and_then(serde_json::Value::as_i64));

        // `response_format` (nearai/cloud-api #668, #720): rides in `extra`.
        // Translate json_object/json_schema into Gemini's native structured-output
        // fields so it returns raw JSON (no markdown fences) and enforces the
        // schema. Strict json_schema goes through `responseJsonSchema` so
        // constraints (`additionalProperties:false`, `$ref`, `$defs`, `oneOf`)
        // are preserved; non-strict uses the lossy OpenAPI-subset `responseSchema`.
        let response_format = params
            .extra
            .get("response_format")
            .map(response_format_to_gemini)
            .unwrap_or_default();

        let generation_config = if params.temperature.is_some()
            || params.top_p.is_some()
            || max_tokens.is_some()
            || params.stop.is_some()
            || seed.is_some()
            || response_format.mime_type.is_some()
            || response_format.schema.is_some()
            || response_format.json_schema.is_some()
        {
            Some(GeminiGenerationConfig {
                temperature: params.temperature,
                top_p: params.top_p,
                max_output_tokens: max_tokens,
                stop_sequences: params.stop.clone(),
                seed,
                response_mime_type: response_format.mime_type,
                response_schema: response_format.schema,
                response_json_schema: response_format.json_schema,
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
                message: crate::extract_error_message(&error_text),
                is_external: true,
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
                message: crate::extract_error_message(&error_text),
                is_external: true,
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

        let openai_response = convert_to_openai_response(gemini_response, model)?;

        // Serialize our normalized response. We intentionally overwrite fields
        // like `usage` (and any future cost-related fields derived from it) instead of passing
        // through native payload directly, to avoid inconsistencies between what we
        // bill on and what we expose on the wire.
        let serialized_bytes = serde_json::to_vec(&openai_response).map_err(|e| {
            CompletionError::CompletionError(format!("Failed to serialize response: {e}"))
        })?;

        Ok(ChatCompletionResponseWithBytes {
            response: openai_response,
            raw_bytes: serialized_bytes,
            serving: crate::ServingProvider::EXTERNAL,
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

    fn base_params() -> ChatCompletionParams {
        ChatCompletionParams {
            placement: Default::default(),
            request_priority: 0,
            model: "gemini-2.5-flash".to_string(),
            messages: vec![crate::ChatMessage {
                reasoning_content: None,
                role: MessageRole::User,
                content: Some(serde_json::Value::String("Hello".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_completion_tokens: None,
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
            seed: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            metadata: None,
            store: None,
            stream_options: None,
            service_tier: None,
            modalities: None,
            original_request: None,
            extra: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_build_request_forwards_token_limit() {
        let backend = GeminiBackend::new();
        for (max_tokens, max_completion_tokens, expected) in [
            (Some(128), None, 128),
            (None, Some(256), 256),
            (Some(128), Some(256), 256),
        ] {
            let mut params = base_params();
            params.max_tokens = max_tokens;
            params.max_completion_tokens = max_completion_tokens;
            let request = backend.build_request(&params);
            let body = serde_json::to_value(&request).unwrap();
            assert_eq!(body["generationConfig"]["maxOutputTokens"], expected);
        }
    }

    // ── #669: seed forwarded to generationConfig.seed ───────────────────────

    #[test]
    fn test_build_request_forwards_seed_from_extra() {
        let backend = GeminiBackend::new();
        let mut params = base_params();
        // The service layer hardcodes the typed `seed` to None and forwards the
        // original in `extra`; we must still pick it up.
        params
            .extra
            .insert("seed".to_string(), serde_json::json!(42));
        let request = backend.build_request(&params);
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["generationConfig"]["seed"], 42);
    }

    #[test]
    fn test_build_request_forwards_typed_seed() {
        let backend = GeminiBackend::new();
        let mut params = base_params();
        params.seed = Some(7);
        let request = backend.build_request(&params);
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["generationConfig"]["seed"], 7);
    }

    #[test]
    fn test_build_request_no_seed_omits_field() {
        let backend = GeminiBackend::new();
        let params = base_params();
        let request = backend.build_request(&params);
        let body = serde_json::to_value(&request).unwrap();
        // No generationConfig at all when nothing is set.
        assert!(body.get("generationConfig").is_none());
    }

    // ── #668: response_format → generationConfig structured output ──────────

    #[test]
    fn test_build_request_json_object_sets_mime_type() {
        let backend = GeminiBackend::new();
        let mut params = base_params();
        params.extra.insert(
            "response_format".to_string(),
            serde_json::json!({"type": "json_object"}),
        );
        let request = backend.build_request(&params);
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert!(body["generationConfig"].get("responseSchema").is_none());
    }

    /// #720: a strict `json_schema` request must be wired to
    /// `generationConfig.responseJsonSchema` with the ORIGINAL schema preserved
    /// (`additionalProperties:false` and friends intact), and must NOT populate
    /// the lossy `responseSchema` field (they are mutually exclusive in Gemini).
    #[test]
    fn test_build_request_strict_json_schema_uses_response_json_schema() {
        let backend = GeminiBackend::new();
        let mut params = base_params();
        params.extra.insert(
            "response_format".to_string(),
            serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "weather",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"city": {"type": "string"}}
                    }
                }
            }),
        );
        let request = backend.build_request(&params);
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        // Lossy field must be absent for strict schemas.
        assert!(body["generationConfig"].get("responseSchema").is_none());
        let schema = &body["generationConfig"]["responseJsonSchema"];
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["city"].is_object());
        // Strict constraint preserved (would have been stripped before #720).
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    }

    /// Non-strict `json_schema` uses the lossy `responseSchema` (best-effort).
    #[test]
    fn test_build_request_non_strict_json_schema_uses_response_schema() {
        let backend = GeminiBackend::new();
        let mut params = base_params();
        params.extra.insert(
            "response_format".to_string(),
            serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "weather",
                    "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"city": {"type": "string"}}
                    }
                }
            }),
        );
        let request = backend.build_request(&params);
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        // Strict JSON-Schema field must be absent for non-strict schemas.
        assert!(body["generationConfig"].get("responseJsonSchema").is_none());
        let schema = &body["generationConfig"]["responseSchema"];
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["city"].is_object());
        // sanitized: Gemini-unsupported keyword removed
        assert!(schema
            .as_object()
            .unwrap()
            .get("additionalProperties")
            .is_none());
    }

    fn type_array_schema() -> serde_json::Value {
        serde_json::json!({
            "type": ["object"],
            "additionalProperties": false,
            "properties": {
                "name": {
                    "type": ["string", "null"],
                    "description": "Optional display name"
                },
                "values": {
                    "type": "array",
                    "items": {
                        "type": ["object"],
                        "properties": {
                            "value": {"type": ["null", "string", "number"]}
                        },
                        "required": ["value"]
                    }
                },
                "status": {"type": ["string"], "enum": ["ready", "pending"]},
                "details": {
                    "type": ["object", "null"],
                    "properties": {"label": {"type": "string"}},
                    "required": ["label"],
                    "propertyOrdering": ["label"]
                },
                "tags": {
                    "type": ["null", "array"],
                    "items": {"type": "string"},
                    "minItems": 1
                },
                "enum_status": {
                    "type": ["string", "null"],
                    "nullable": true,
                    "enum": ["ready", "pending"]
                },
                "choice": {
                    "anyOf": [
                        {"type": ["boolean", "null"]},
                        {"type": ["integer"]}
                    ]
                }
            },
            "required": ["name", "values"]
        })
    }

    fn normalized_type_array_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "nullable": true,
                    "description": "Optional display name"
                },
                "values": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "value": {
                                "anyOf": [
                                    {"type": "null"}, {"type": "string"}, {"type": "number"}
                                ]
                            }
                        },
                        "required": ["value"]
                    }
                },
                "status": {"type": "string", "enum": ["ready", "pending"]},
                "details": {
                    "type": "object",
                    "nullable": true,
                    "properties": {"label": {"type": "string"}},
                    "required": ["label"],
                    "propertyOrdering": ["label"]
                },
                "tags": {
                    "type": "array",
                    "nullable": true,
                    "items": {"type": "string"},
                    "minItems": 1
                },
                "enum_status": {"type": "string", "enum": ["ready", "pending"]},
                "choice": {
                    "anyOf": [
                        {"type": "boolean", "nullable": true},
                        {"type": "integer"}
                    ]
                }
            },
            "required": ["name", "values"]
        })
    }

    /// #1189: the serialized tool declaration must use scalar types, including
    /// nullable properties and unions nested beneath items and anyOf.
    /// FunctionDefinition does not retain tool strict; this covers schema conversion.
    #[test]
    fn test_build_request_normalizes_tool_parameter_type_arrays() {
        let backend = GeminiBackend::new();
        let tool = serde_json::json!({
            "type": "function",
            "function": {
                "name": "save_record",
                "description": "Save a record",
                "parameters": type_array_schema()
            }
        });
        let mut params = base_params();
        params.tools = Some(vec![serde_json::from_value(tool).unwrap()]);

        let body = serde_json::to_value(backend.build_request(&params)).unwrap();
        let declaration = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(declaration["name"], "save_record");
        assert_eq!(declaration["description"], "Save a record");
        assert_eq!(declaration["parameters"], normalized_type_array_schema());
    }

    #[test]
    fn test_build_request_normalizes_non_strict_response_schema_type_arrays() {
        let backend = GeminiBackend::new();
        for strict in [None, Some(false)] {
            let mut response_format = serde_json::json!({
                "type": "json_schema",
                "json_schema": {"name": "record", "schema": type_array_schema()}
            });
            if let Some(strict) = strict {
                response_format["json_schema"]["strict"] = serde_json::json!(strict);
            }
            let mut params = base_params();
            params
                .extra
                .insert("response_format".to_string(), response_format);

            let body = serde_json::to_value(backend.build_request(&params)).unwrap();
            let config = &body["generationConfig"];
            assert_eq!(config["responseMimeType"], "application/json");
            assert_eq!(config["responseSchema"], normalized_type_array_schema());
            assert!(config.get("responseJsonSchema").is_none());
        }
    }

    #[test]
    fn test_build_request_preserves_strict_response_schema_type_arrays() {
        let backend = GeminiBackend::new();
        let original_schema = type_array_schema();
        let mut params = base_params();
        params.extra.insert(
            "response_format".to_string(),
            serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "record",
                    "strict": true,
                    "schema": original_schema.clone()
                }
            }),
        );

        let body = serde_json::to_value(backend.build_request(&params)).unwrap();
        let config = &body["generationConfig"];
        assert_eq!(config["responseMimeType"], "application/json");
        assert_eq!(config["responseJsonSchema"], original_schema);
        assert!(config.get("responseSchema").is_none());
    }

    #[test]
    fn test_build_request_type_intersection_does_not_restore_null() {
        let backend = GeminiBackend::new();
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": ["string", "boolean"],
                    "anyOf": [{"type": ["string", "null"]}]
                }
            },
            "required": ["value"]
        });
        let expected = serde_json::json!({
            "type": "object",
            "properties": {
                "value": {"anyOf": [{"anyOf": [{"type": "string"}]}]}
            },
            "required": ["value"]
        });
        let mut params = base_params();
        params.tools = Some(vec![serde_json::from_value(serde_json::json!({
            "type": "function",
            "function": {"name": "save_value", "parameters": schema.clone()}
        }))
        .unwrap()]);
        params.extra.insert(
            "response_format".to_string(),
            serde_json::json!({
                "type": "json_schema",
                "json_schema": {"name": "value", "schema": schema}
            }),
        );

        let body = serde_json::to_value(backend.build_request(&params)).unwrap();
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parameters"],
            expected
        );
        assert_eq!(body["generationConfig"]["responseSchema"], expected);
    }

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

    // End-to-end conversion: a MAX_TOKENS-with-empty-content payload from Google
    // produces an OpenAI-shaped response with `content: null` and
    // `finish_reason: "length"`. Locks in the user-visible contract.
    #[test]
    fn test_convert_to_openai_response_max_tokens_empty_content() {
        let json = r#"{
            "candidates": [{
                "content": {},
                "finishReason": "MAX_TOKENS",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 7,
                "candidatesTokenCount": 0,
                "totalTokenCount": 7
            }
        }"#;
        let response: GeminiResponse = serde_json::from_str(json).unwrap();
        let openai = convert_to_openai_response(response, "google/gemini-3-pro").unwrap();

        assert_eq!(openai.choices.len(), 1);
        let choice = &openai.choices[0];
        assert!(choice.message.content.is_none());
        assert!(choice.message.tool_calls.is_none());
        assert_eq!(choice.finish_reason.as_deref(), Some("length"));
        assert_eq!(openai.usage.prompt_tokens, 7);
        assert_eq!(openai.usage.completion_tokens, 0);
        assert_eq!(openai.model, "google/gemini-3-pro");
    }

    // Same contract when the entire `content` field is absent (safety-block shape).
    #[test]
    fn test_convert_to_openai_response_missing_content() {
        let json = r#"{
            "candidates": [{
                "finishReason": "SAFETY",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 9,
                "totalTokenCount": 9
            }
        }"#;
        let response: GeminiResponse = serde_json::from_str(json).unwrap();
        let openai = convert_to_openai_response(response, "google/gemini-3-pro").unwrap();
        let choice = &openai.choices[0];
        assert!(choice.message.content.is_none());
        assert!(choice.message.tool_calls.is_none());
        // SAFETY maps to "content_filter" via map_finish_reason_string.
        assert!(choice.finish_reason.is_some());
    }

    // Normal STOP response with text round-trips into a populated content field.
    #[test]
    fn test_convert_to_openai_response_normal_stop() {
        let json = r#"{
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hi!"}]},
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 3,
                "candidatesTokenCount": 2,
                "totalTokenCount": 5
            }
        }"#;
        let response: GeminiResponse = serde_json::from_str(json).unwrap();
        let openai = convert_to_openai_response(response, "google/gemini-3-pro").unwrap();
        let choice = &openai.choices[0];
        assert_eq!(choice.message.content.as_deref(), Some("Hi!"));
        assert_eq!(choice.finish_reason.as_deref(), Some("stop"));
    }

    // Billing: thinking is reported outside `candidatesTokenCount` and billed
    // by Google at the output rate. Counts are from a `gemini-2.5-pro` reply to
    // a short arithmetic prompt (26 visible tokens, 2,706 thinking tokens).
    #[test]
    fn test_convert_to_openai_response_counts_thinking_and_cache() {
        let json = r#"{
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "42"}]},
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 71,
                "cachedContentTokenCount": 64,
                "candidatesTokenCount": 26,
                "thoughtsTokenCount": 2706,
                "totalTokenCount": 2803
            }
        }"#;
        let response: GeminiResponse = serde_json::from_str(json).unwrap();
        let openai = convert_to_openai_response(response, "google/gemini-2.5-pro").unwrap();

        assert_eq!(openai.usage.prompt_tokens, 71);
        assert_eq!(openai.usage.completion_tokens, 2732);
        assert_eq!(openai.usage.total_tokens, 2803);
        assert_eq!(openai.usage.cached_tokens(), 64);
        assert_eq!(openai.usage.reasoning_tokens(), Some(2706));

        // What the client sees.
        let wire = serde_json::to_value(&openai).unwrap();
        assert_eq!(
            wire["usage"],
            serde_json::json!({
                "prompt_tokens": 71,
                "completion_tokens": 2732,
                "total_tokens": 2803,
                "prompt_tokens_details": {"cached_tokens": 64},
                "completion_tokens_details": {"reasoning_tokens": 2706}
            })
        );
    }

    #[test]
    fn test_convert_to_openai_response_usage_without_thinking_or_cache() {
        let json = r#"{
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hi!"}]},
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 3,
                "candidatesTokenCount": 2,
                "totalTokenCount": 5
            }
        }"#;
        let response: GeminiResponse = serde_json::from_str(json).unwrap();
        let openai = convert_to_openai_response(response, "google/gemini-2.0-flash").unwrap();

        let wire = serde_json::to_value(&openai).unwrap();
        assert_eq!(
            wire["usage"],
            serde_json::json!({
                "prompt_tokens": 3,
                "completion_tokens": 2,
                "total_tokens": 5
            })
        );
    }

    /// Full HTTP path, non-streaming and streaming, against a stand-in for
    /// Google's API: both must report the same usage for the same request.
    #[tokio::test]
    async fn test_backend_reports_thinking_and_cache_on_both_paths() {
        use futures_util::StreamExt;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let usage_metadata = serde_json::json!({
            "promptTokenCount": 71,
            "cachedContentTokenCount": 64,
            "candidatesTokenCount": 26,
            "thoughtsTokenCount": 2706,
            "totalTokenCount": 2803
        });
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-pro:generateContent"))
            .and(header("x-goog-api-key", "upstream-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{
                    "content": {"role": "model", "parts": [{"text": "42"}]},
                    "finishReason": "STOP"
                }],
                "usageMetadata": usage_metadata
            })))
            .mount(&server)
            .await;
        let sse = format!(
            "data: {}\n\ndata: {}\n\n",
            serde_json::json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": "4"}]}}],
                "usageMetadata": {
                    "promptTokenCount": 71,
                    "cachedContentTokenCount": 64,
                    "candidatesTokenCount": 1,
                    "thoughtsTokenCount": 2706,
                    "totalTokenCount": 2778
                }
            }),
            serde_json::json!({
                "candidates": [{
                    "content": {"role": "model", "parts": [{"text": "2"}]},
                    "finishReason": "STOP"
                }],
                "usageMetadata": usage_metadata
            }),
        );
        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-pro:streamGenerateContent"))
            .and(header("x-goog-api-key", "upstream-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .mount(&server)
            .await;

        let config = BackendConfig {
            base_url: server.uri(),
            api_key: "upstream-secret".to_string(),
            ..Default::default()
        };
        let backend = GeminiBackend::new();

        let response = backend
            .chat_completion(&config, "google/gemini-2.5-pro", base_params())
            .await
            .expect("completion should succeed");
        let non_streaming = response.response.usage;

        let mut stream = backend
            .chat_completion_stream(&config, "google/gemini-2.5-pro", base_params())
            .await
            .expect("stream should start");
        let mut streaming = None;
        while let Some(event) = stream.next().await {
            let event = event.expect("stream event should parse");
            if let Some(crate::StreamChunk::Chat(chunk)) = event.chunk {
                if chunk.usage.is_some() {
                    streaming = chunk.usage;
                }
            }
        }
        let streaming = streaming.expect("stream should carry usage");

        for usage in [&non_streaming, &streaming] {
            assert_eq!(usage.prompt_tokens, 71);
            assert_eq!(usage.completion_tokens, 2732);
            assert_eq!(usage.total_tokens, 2803);
            assert_eq!(usage.cached_tokens(), 64);
            assert_eq!(usage.reasoning_tokens(), Some(2706));
        }
    }
}
