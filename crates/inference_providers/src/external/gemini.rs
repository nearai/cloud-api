//! Gemini backend implementation
//!
//! This backend handles Google's Gemini API, translating between our
//! OpenAI-compatible format and Gemini's native format.

use super::backend::{BackendConfig, ExternalBackend};
use crate::{
    AudioError, AudioSpeechParams, AudioSpeechResponseWithBytes, AudioTranscriptionParams,
    AudioTranscriptionResponse, AudioTranscriptionResponseWithBytes, BufferedSSEParser, ChatChoice,
    ChatCompletionChunk, ChatCompletionParams, ChatCompletionResponse,
    ChatCompletionResponseChoice, ChatCompletionResponseWithBytes, ChatDelta, ChatResponseMessage,
    CompletionError, ImageData, ImageGenerationError, ImageGenerationParams,
    ImageGenerationResponse, ImageGenerationResponseWithBytes, MessageRole, SSEEventParser,
    StreamChunk, StreamingResult, TokenUsage,
};
use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures_util::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Gemini backend
///
/// Translates between OpenAI-compatible format and Google Gemini's API.
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

    /// Convert OpenAI messages to Gemini format
    fn convert_messages(
        messages: &[crate::ChatMessage],
    ) -> (Option<GeminiSystemInstruction>, Vec<GeminiContent>) {
        // Helper to extract string content from serde_json::Value
        let extract_content = |value: &serde_json::Value| -> String {
            match value {
                serde_json::Value::String(s) => s.clone(),
                _ => value.to_string(),
            }
        };

        let mut system_instruction = None;
        let mut contents = Vec::new();

        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    // Gemini uses systemInstruction
                    if let Some(content) = &msg.content {
                        system_instruction = Some(GeminiSystemInstruction {
                            parts: vec![GeminiPart {
                                text: extract_content(content),
                            }],
                        });
                    }
                }
                MessageRole::User => {
                    contents.push(GeminiContent {
                        role: "user".to_string(),
                        parts: vec![GeminiPart {
                            text: msg
                                .content
                                .as_ref()
                                .map(&extract_content)
                                .unwrap_or_default(),
                        }],
                    });
                }
                MessageRole::Assistant => {
                    // Gemini uses "model" role for assistant
                    contents.push(GeminiContent {
                        role: "model".to_string(),
                        parts: vec![GeminiPart {
                            text: msg
                                .content
                                .as_ref()
                                .map(&extract_content)
                                .unwrap_or_default(),
                        }],
                    });
                }
                MessageRole::Tool => {
                    // Tool results go as user messages
                    contents.push(GeminiContent {
                        role: "user".to_string(),
                        parts: vec![GeminiPart {
                            text: msg
                                .content
                                .as_ref()
                                .map(&extract_content)
                                .unwrap_or_default(),
                        }],
                    });
                }
            }
        }

        (system_instruction, contents)
    }
}

impl Default for GeminiBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Map Gemini's finishReason to OpenAI-compatible finish_reason (enum for streaming)
///
/// Gemini uses: "STOP", "MAX_TOKENS", "SAFETY", "RECITATION", "OTHER"
/// OpenAI uses: "stop", "length", "content_filter", "tool_calls"
fn map_finish_reason(finish_reason: Option<&String>) -> Option<crate::FinishReason> {
    finish_reason.map(|r| match r.as_str() {
        "STOP" => crate::FinishReason::Stop,
        "MAX_TOKENS" => crate::FinishReason::Length,
        "SAFETY" => crate::FinishReason::ContentFilter,
        _ => crate::FinishReason::Stop,
    })
}

/// Map Gemini's finishReason to OpenAI-compatible finish_reason (string for non-streaming)
fn map_finish_reason_string(finish_reason: Option<&String>) -> Option<String> {
    finish_reason.map(|r| match r.as_str() {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        "SAFETY" => "content_filter".to_string(),
        _ => "stop".to_string(),
    })
}

/// Gemini part format
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiPart {
    text: String,
}

/// Gemini content format
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

/// Gemini system instruction
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

/// Gemini generation config
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
}

/// Gemini request format
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

/// Gemini response candidate
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GeminiCandidate {
    content: GeminiContent,
    finish_reason: Option<String>,
    #[serde(default)]
    safety_ratings: Vec<serde_json::Value>,
}

/// Gemini usage metadata
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    #[serde(default)]
    prompt_token_count: i32,
    #[serde(default)]
    candidates_token_count: i32,
    #[serde(default)]
    total_token_count: i32,
}

/// Gemini response format
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    usage_metadata: GeminiUsageMetadata,
    model_version: Option<String>,
}

// ==================== Gemini Image Generation (Imagen) Structures ====================

/// Gemini/Imagen image generation request
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

/// Gemini/Imagen image generation response
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiImageGenerationResponse {
    #[serde(default)]
    generated_images: Vec<GeminiGeneratedImage>,
}

/// Generated image from Imagen
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGeneratedImage {
    image: GeminiImageData,
}

/// Image data from Imagen
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiImageData {
    /// Base64-encoded image bytes
    image_bytes: String,
}

// ==================== Google Cloud Text-to-Speech Structures ====================

/// Google Cloud TTS synthesis request
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GoogleTtsRequest {
    input: GoogleTtsInput,
    voice: GoogleTtsVoice,
    audio_config: GoogleTtsAudioConfig,
}

/// Input for TTS synthesis
#[derive(Debug, Clone, Serialize)]
struct GoogleTtsInput {
    text: String,
}

/// Voice selection for TTS
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GoogleTtsVoice {
    language_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssml_gender: Option<String>,
}

/// Audio configuration for TTS output
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GoogleTtsAudioConfig {
    audio_encoding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    speaking_rate: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pitch: Option<f32>,
}

/// Google Cloud TTS synthesis response
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleTtsResponse {
    audio_content: String,
}

// ==================== Google Cloud Speech-to-Text Structures ====================

/// Google Cloud STT recognition request
#[derive(Debug, Clone, Serialize)]
struct GoogleSttRequest {
    config: GoogleSttConfig,
    audio: GoogleSttAudio,
}

/// STT recognition configuration
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GoogleSttConfig {
    encoding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_rate_hertz: Option<i32>,
    language_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_automatic_punctuation: Option<bool>,
}

/// Audio data for STT
#[derive(Debug, Clone, Serialize)]
struct GoogleSttAudio {
    content: String,
}

/// Google Cloud STT recognition response
#[derive(Debug, Clone, Deserialize)]
struct GoogleSttResponse {
    #[serde(default)]
    results: Vec<GoogleSttResult>,
}

/// STT recognition result
#[derive(Debug, Clone, Deserialize)]
struct GoogleSttResult {
    #[serde(default)]
    alternatives: Vec<GoogleSttAlternative>,
}

/// STT recognition alternative
#[derive(Debug, Clone, Deserialize)]
struct GoogleSttAlternative {
    transcript: String,
    #[serde(default)]
    #[allow(dead_code)]
    confidence: f32,
}

/// Map voice name to Google Cloud TTS voice parameters
fn map_voice_to_google_tts(voice: &str) -> (String, Option<String>, Option<String>) {
    // Map OpenAI-style voices to Google Cloud TTS voices
    // Format: (language_code, voice_name, ssml_gender)
    match voice.to_lowercase().as_str() {
        "alloy" => (
            "en-US".to_string(),
            Some("en-US-Neural2-D".to_string()),
            None,
        ),
        "echo" => (
            "en-US".to_string(),
            Some("en-US-Neural2-A".to_string()),
            None,
        ),
        "fable" => (
            "en-GB".to_string(),
            Some("en-GB-Neural2-B".to_string()),
            None,
        ),
        "onyx" => (
            "en-US".to_string(),
            Some("en-US-Neural2-J".to_string()),
            None,
        ),
        "nova" => (
            "en-US".to_string(),
            Some("en-US-Neural2-F".to_string()),
            None,
        ),
        "shimmer" => (
            "en-US".to_string(),
            Some("en-US-Neural2-C".to_string()),
            None,
        ),
        // If it looks like a Google voice name (contains language code), use it directly
        v if v.contains("-") && (v.contains("en-") || v.contains("es-") || v.contains("fr-")) => {
            let parts: Vec<&str> = v.split('-').collect();
            if parts.len() >= 2 {
                let lang = format!("{}-{}", parts[0], parts[1].to_uppercase());
                (lang, Some(v.to_string()), None)
            } else {
                ("en-US".to_string(), Some(v.to_string()), None)
            }
        }
        _ => ("en-US".to_string(), None, Some("NEUTRAL".to_string())),
    }
}

/// Map OpenAI audio format to Google Cloud encoding
fn map_audio_format_to_google(format: Option<&String>) -> String {
    match format.map(|s| s.to_lowercase()).as_deref() {
        Some("mp3") => "MP3".to_string(),
        Some("opus") => "OGG_OPUS".to_string(),
        Some("aac") => "MP3".to_string(), // Google doesn't support AAC, fallback to MP3
        Some("flac") => "FLAC".to_string(),
        Some("wav") => "LINEAR16".to_string(),
        Some("pcm") => "LINEAR16".to_string(),
        _ => "MP3".to_string(), // Default to MP3
    }
}

/// Get content type from Google encoding
fn google_encoding_to_content_type(encoding: &str) -> &'static str {
    match encoding {
        "MP3" => "audio/mpeg",
        "OGG_OPUS" => "audio/ogg",
        "FLAC" => "audio/flac",
        "LINEAR16" => "audio/wav",
        _ => "audio/mpeg",
    }
}

/// Map audio file extension to Google STT encoding
fn map_file_extension_to_google_stt_encoding(filename: &str) -> String {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "mp3" => "MP3".to_string(),
        "wav" => "LINEAR16".to_string(),
        "flac" => "FLAC".to_string(),
        "ogg" => "OGG_OPUS".to_string(),
        "webm" => "WEBM_OPUS".to_string(),
        "m4a" => "MP3".to_string(),  // Fallback
        _ => "LINEAR16".to_string(), // Default
    }
}

/// Strip vendor prefix from model name (e.g., "google/gemini-2.0-flash" -> "gemini-2.0-flash")
///
/// Gemini API expects model names without vendor prefixes in the URL path.
/// Model names in our system may include prefixes like "google/" for routing purposes.
fn strip_vendor_prefix(model: &str) -> &str {
    // Handle common vendor prefixes
    if let Some(stripped) = model.strip_prefix("google/") {
        return stripped;
    }
    if let Some(stripped) = model.strip_prefix("vertex/") {
        return stripped;
    }
    // Return as-is if no known prefix
    model
}

/// Convert size string (e.g., "1024x1024") to Gemini aspect ratio
fn size_to_aspect_ratio(size: Option<&String>) -> Option<String> {
    size.and_then(|s| {
        let parts: Vec<&str> = s.split('x').collect();
        if parts.len() == 2 {
            if let (Ok(w), Ok(h)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                // Calculate closest aspect ratio
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

/// State for Gemini SSE parsing
pub struct GeminiParserState {
    pub(crate) model: String,
    /// Unique request ID (UUID-based to ensure uniqueness across concurrent requests)
    pub(crate) request_id: String,
    pub(crate) created: i64,
    pub(crate) chunk_index: i64,
    pub(crate) accumulated_prompt_tokens: i32,
    pub(crate) accumulated_completion_tokens: i32,
}

impl GeminiParserState {
    pub fn new(model: String) -> Self {
        Self {
            model,
            request_id: format!("gemini-{}", Uuid::new_v4()),
            created: chrono::Utc::now().timestamp(),
            chunk_index: 0,
            accumulated_prompt_tokens: 0,
            accumulated_completion_tokens: 0,
        }
    }
}

/// Gemini event parser
///
/// Handles Gemini's SSE/JSON streaming format and converts to OpenAI-compatible chunks.
pub struct GeminiEventParser;

impl SSEEventParser for GeminiEventParser {
    type State = GeminiParserState;

    fn parse_event(
        state: &mut Self::State,
        data: &str,
    ) -> Result<Option<StreamChunk>, CompletionError> {
        // Don't include parse error details - may contain customer data
        let response: GeminiResponse = serde_json::from_str(data)
            .map_err(|_| CompletionError::InvalidResponse("Failed to parse event".to_string()))?;

        if response.candidates.is_empty() {
            return Ok(None);
        }

        let candidate = &response.candidates[0];
        let text = candidate
            .content
            .parts
            .iter()
            .map(|p| p.text.clone())
            .collect::<Vec<_>>()
            .join("");

        // Update token counts
        state.accumulated_prompt_tokens = response.usage_metadata.prompt_token_count;
        state.accumulated_completion_tokens = response.usage_metadata.candidates_token_count;

        let finish_reason = map_finish_reason(candidate.finish_reason.as_ref());

        let is_first = state.chunk_index == 0;
        state.chunk_index += 1;

        let chunk = ChatCompletionChunk {
            id: state.request_id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: state.created,
            model: state.model.clone(),
            system_fingerprint: None,
            modality: None,
            choices: vec![ChatChoice {
                index: 0,
                delta: Some(ChatDelta {
                    role: if is_first {
                        Some(MessageRole::Assistant)
                    } else {
                        None
                    },
                    content: if text.is_empty() { None } else { Some(text) },
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    reasoning_content: None,
                    reasoning: None,
                }),
                logprobs: None,
                finish_reason,
                token_ids: None,
            }],
            usage: Some(TokenUsage {
                prompt_tokens: state.accumulated_prompt_tokens,
                completion_tokens: state.accumulated_completion_tokens,
                total_tokens: state.accumulated_prompt_tokens + state.accumulated_completion_tokens,
                prompt_tokens_details: None,
            }),
            prompt_token_ids: None,
        };

        Ok(Some(StreamChunk::Chat(chunk)))
    }

    /// Gemini can return raw JSON lines (not just SSE format)
    fn handles_raw_json() -> bool {
        true
    }
}

/// SSE parser for Gemini's streaming format
///
/// Type alias using the generic BufferedSSEParser with Gemini-specific event parsing.
pub type GeminiSSEParser<S> = BufferedSSEParser<S, GeminiEventParser>;

/// Create a new Gemini SSE parser
pub fn new_gemini_sse_parser<S>(stream: S, model: String) -> GeminiSSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    BufferedSSEParser::new(stream, GeminiParserState::new(model))
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
        // Strip vendor prefix from model name (e.g., "google/gemini-2.0-flash" -> "gemini-2.0-flash")
        let model_name = strip_vendor_prefix(model);

        // Gemini API URL format: {base_url}/models/{model}:streamGenerateContent?alt=sse
        // API key is passed via x-goog-api-key header for security
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            config.base_url, model_name
        );

        let (system_instruction, contents) = Self::convert_messages(&params.messages);

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
                stop_sequences: params.stop,
            })
        } else {
            None
        };

        let request = GeminiRequest {
            contents,
            system_instruction,
            generation_config,
        };

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
            let status = response.status();
            let status_code = status.as_u16();
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
        // Strip vendor prefix from model name (e.g., "google/gemini-2.0-flash" -> "gemini-2.0-flash")
        let model_name = strip_vendor_prefix(model);

        // Gemini API URL format: {base_url}/models/{model}:generateContent
        // API key is passed via x-goog-api-key header for security
        let url = format!("{}/models/{}:generateContent", config.base_url, model_name);

        let (system_instruction, contents) = Self::convert_messages(&params.messages);

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
                stop_sequences: params.stop,
            })
        } else {
            None
        };

        let request = GeminiRequest {
            contents,
            system_instruction,
            generation_config,
        };

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
            let status = response.status();
            let status_code = status.as_u16();
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
        let content = candidate
            .content
            .parts
            .iter()
            .map(|p| p.text.clone())
            .collect::<Vec<_>>()
            .join("");

        let openai_response = ChatCompletionResponse {
            id: format!("gemini-{}", Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatResponseMessage {
                    role: MessageRole::Assistant,
                    content: Some(content),
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls: None,
                    reasoning_content: None,
                    reasoning: None,
                },
                logprobs: None,
                finish_reason: map_finish_reason_string(candidate.finish_reason.as_ref()),
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

        // Re-serialize for consistent raw bytes
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
        // Strip vendor prefix from model name (e.g., "google/imagen-3.0" -> "imagen-3.0")
        let model_name = strip_vendor_prefix(model);

        // Gemini uses the generateImage endpoint for Imagen models
        // URL format: {base_url}/models/{model}:generateImages
        let url = format!("{}/models/{}:generateImages", config.base_url, model_name);

        // Convert OpenAI-style params to Gemini format
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

        // Get raw bytes first
        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e| ImageGenerationError::GenerationError(e.to_string()))?
            .to_vec();

        // Parse Gemini response
        let gemini_response: GeminiImageGenerationResponse = serde_json::from_slice(&raw_bytes)
            .map_err(|e| {
                ImageGenerationError::GenerationError(format!("Failed to parse response: {e}"))
            })?;

        // Convert to OpenAI-compatible format
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

        // Re-serialize for consistent raw bytes
        let serialized_bytes = serde_json::to_vec(&openai_response).map_err(|e| {
            ImageGenerationError::GenerationError(format!("Failed to serialize response: {e}"))
        })?;

        Ok(ImageGenerationResponseWithBytes {
            response: openai_response,
            raw_bytes: serialized_bytes,
        })
    }

    async fn audio_transcription(
        &self,
        config: &BackendConfig,
        _model: &str,
        params: AudioTranscriptionParams,
    ) -> Result<AudioTranscriptionResponseWithBytes, AudioError> {
        // Google Cloud Speech-to-Text API endpoint
        let url = "https://speech.googleapis.com/v1/speech:recognize";

        // Encode audio data to base64
        let audio_content = base64::engine::general_purpose::STANDARD.encode(&params.audio_data);

        // Determine encoding from filename
        let encoding = map_file_extension_to_google_stt_encoding(&params.filename);

        // Build request
        // Use provided sample rate or default to 16000 Hz (standard for speech-to-text)
        let sample_rate_hertz = params.sample_rate_hertz.unwrap_or(16000) as i32;

        let stt_config = GoogleSttConfig {
            encoding,
            sample_rate_hertz: Some(sample_rate_hertz),
            language_code: params
                .language
                .clone()
                .unwrap_or_else(|| "en-US".to_string()),
            enable_automatic_punctuation: Some(true),
        };

        let request = GoogleSttRequest {
            config: stt_config,
            audio: GoogleSttAudio {
                content: audio_content,
            },
        };

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Content-Type",
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            "x-goog-api-key",
            reqwest::header::HeaderValue::from_str(&config.api_key).map_err(|e| {
                AudioError::HttpError {
                    status_code: 0,
                    message: format!("Invalid API key: {e}"),
                }
            })?,
        );

        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = self
            .client
            .post(url)
            .headers(headers)
            .timeout(timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e: reqwest::Error| AudioError::HttpError {
                status_code: 0,
                message: e.to_string(),
            })?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(AudioError::HttpError {
                status_code,
                message,
            });
        }

        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e: reqwest::Error| AudioError::HttpError {
                status_code: 0,
                message: e.to_string(),
            })?
            .to_vec();

        let stt_response: GoogleSttResponse = serde_json::from_slice(&raw_bytes).map_err(|e| {
            AudioError::TranscriptionFailed(format!("Failed to parse STT response: {e}"))
        })?;

        // Extract transcript from results
        let transcript = stt_response
            .results
            .iter()
            .filter_map(|r| r.alternatives.first())
            .map(|a| a.transcript.clone())
            .collect::<Vec<_>>()
            .join(" ");

        let response = AudioTranscriptionResponse {
            text: transcript,
            task: Some("transcribe".to_string()),
            language: params.language,
            duration: None,
            words: None,
            segments: None,
            id: None,
        };

        // Re-serialize for consistent raw bytes
        let serialized_bytes = serde_json::to_vec(&response).map_err(|e| {
            AudioError::TranscriptionFailed(format!("Failed to serialize response: {e}"))
        })?;

        Ok(AudioTranscriptionResponseWithBytes {
            response,
            raw_bytes: serialized_bytes,
            audio_duration_seconds: None,
        })
    }

    async fn audio_speech(
        &self,
        config: &BackendConfig,
        _model: &str,
        params: AudioSpeechParams,
    ) -> Result<AudioSpeechResponseWithBytes, AudioError> {
        // Google Cloud Text-to-Speech API endpoint
        let url = "https://texttospeech.googleapis.com/v1/text:synthesize";

        // Map voice to Google TTS parameters
        let (language_code, voice_name, ssml_gender) = map_voice_to_google_tts(&params.voice);

        // Map audio format
        let audio_encoding = map_audio_format_to_google(params.response_format.as_ref());
        let content_type = google_encoding_to_content_type(&audio_encoding);

        let request = GoogleTtsRequest {
            input: GoogleTtsInput {
                text: params.input.clone(),
            },
            voice: GoogleTtsVoice {
                language_code,
                name: voice_name,
                ssml_gender,
            },
            audio_config: GoogleTtsAudioConfig {
                audio_encoding: audio_encoding.clone(),
                speaking_rate: params.speed,
                pitch: None,
            },
        };

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Content-Type",
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            "x-goog-api-key",
            reqwest::header::HeaderValue::from_str(&config.api_key).map_err(|e| {
                AudioError::HttpError {
                    status_code: 0,
                    message: format!("Invalid API key: {e}"),
                }
            })?,
        );

        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let response = self
            .client
            .post(url)
            .headers(headers)
            .timeout(timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e: reqwest::Error| AudioError::HttpError {
                status_code: 0,
                message: e.to_string(),
            })?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(AudioError::HttpError {
                status_code,
                message,
            });
        }

        let raw_bytes = response
            .bytes()
            .await
            .map_err(|e: reqwest::Error| AudioError::HttpError {
                status_code: 0,
                message: e.to_string(),
            })?
            .to_vec();

        let tts_response: GoogleTtsResponse = serde_json::from_slice(&raw_bytes).map_err(|e| {
            AudioError::SynthesisFailed(format!("Failed to parse TTS response: {e}"))
        })?;

        // Decode base64 audio content
        let audio_data = base64::engine::general_purpose::STANDARD
            .decode(&tts_response.audio_content)
            .map_err(|e| AudioError::SynthesisFailed(format!("Failed to decode audio: {e}")))?;

        Ok(AudioSpeechResponseWithBytes {
            audio_data,
            content_type: content_type.to_string(),
            raw_bytes,
            character_count: params.input.len() as i32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatMessage;

    /// Helper to create a string content value for tests
    fn str_content(s: &str) -> serde_json::Value {
        serde_json::Value::String(s.to_string())
    }

    // ==================== Message Translation Tests ====================

    #[test]
    fn test_convert_messages_extracts_system_instruction() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(str_content("You are a helpful assistant.")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(str_content("Hello")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_some());
        let sys = system_instruction.unwrap();
        assert_eq!(sys.parts.len(), 1);
        assert_eq!(sys.parts[0].text, "You are a helpful assistant.");

        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[0].parts[0].text, "Hello");
    }

    #[test]
    fn test_convert_messages_assistant_becomes_model() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::User,
                content: Some(str_content("Hello")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::Assistant,
                content: Some(str_content("Hi there!")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_none());
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[1].role, "model"); // assistant -> model
        assert_eq!(contents[1].parts[0].text, "Hi there!");
    }

    #[test]
    fn test_convert_messages_empty() {
        let messages: Vec<ChatMessage> = vec![];
        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_none());
        assert!(contents.is_empty());
    }

    #[test]
    fn test_convert_messages_only_system() {
        let messages = vec![ChatMessage {
            role: MessageRole::System,
            content: Some(str_content("You are a bot.")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_some());
        assert!(contents.is_empty());
    }

    #[test]
    fn test_convert_messages_tool_becomes_user() {
        let messages = vec![ChatMessage {
            role: MessageRole::Tool,
            content: Some(str_content("Tool result here")),
            name: None,
            tool_call_id: Some("call_123".to_string()),
            tool_calls: None,
        }];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_none());
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[0].parts[0].text, "Tool result here");
    }

    #[test]
    fn test_convert_messages_none_content() {
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_none());
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].parts[0].text, ""); // Empty string for None
    }

    #[test]
    fn test_convert_messages_multiple_system_uses_last() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(str_content("First system")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(str_content("Hello")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::System,
                content: Some(str_content("Second system")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        // Last system message should be used
        assert!(system_instruction.is_some());
        let sys = system_instruction.unwrap();
        assert_eq!(sys.parts[0].text, "Second system");
        assert_eq!(contents.len(), 1);
    }

    #[test]
    fn test_convert_messages_no_system() {
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: Some(str_content("Hello")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (system_instruction, contents) = GeminiBackend::convert_messages(&messages);

        assert!(system_instruction.is_none());
        assert_eq!(contents.len(), 1);
    }

    // ==================== Response Parsing Tests ====================

    #[test]
    fn test_parse_gemini_response() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello! How can I help you?"}]
                },
                "finishReason": "STOP",
                "safetyRatings": []
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 8,
                "totalTokenCount": 18
            },
            "modelVersion": "gemini-1.5-pro"
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.candidates.len(), 1);
        assert_eq!(response.candidates[0].content.role, "model");
        assert_eq!(
            response.candidates[0].content.parts[0].text,
            "Hello! How can I help you?"
        );
        assert_eq!(
            response.candidates[0].finish_reason,
            Some("STOP".to_string())
        );
        assert_eq!(response.usage_metadata.prompt_token_count, 10);
        assert_eq!(response.usage_metadata.candidates_token_count, 8);
        assert_eq!(response.usage_metadata.total_token_count, 18);
    }

    #[test]
    fn test_parse_gemini_response_empty_candidates() {
        let json = r#"{
            "candidates": [],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 0,
                "totalTokenCount": 10
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();

        assert!(response.candidates.is_empty());
    }

    #[test]
    fn test_parse_gemini_response_multiple_parts() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "First part. "},
                        {"text": "Second part."}
                    ]
                },
                "finishReason": "STOP",
                "safetyRatings": []
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 8,
                "totalTokenCount": 18
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.candidates[0].content.parts.len(), 2);
    }

    #[test]
    fn test_parse_gemini_response_safety_finish_reason() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": ""}]
                },
                "finishReason": "SAFETY",
                "safetyRatings": [{"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "probability": "HIGH"}]
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 0,
                "totalTokenCount": 10
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();

        assert_eq!(
            response.candidates[0].finish_reason,
            Some("SAFETY".to_string())
        );
    }

    // ==================== Finish Reason Mapping Tests ====================

    #[test]
    fn test_finish_reason_mapping() {
        let test_cases = vec![
            ("STOP", crate::FinishReason::Stop),
            ("MAX_TOKENS", crate::FinishReason::Length),
            ("SAFETY", crate::FinishReason::ContentFilter),
            ("UNKNOWN", crate::FinishReason::Stop), // Default
        ];

        for (gemini_reason, expected) in test_cases {
            let reason_string = gemini_reason.to_string();
            let mapped = map_finish_reason(Some(&reason_string)).unwrap();
            assert_eq!(mapped, expected, "Failed for reason: {}", gemini_reason);
        }

        // Test None case
        assert_eq!(map_finish_reason(None), None);
    }

    // ==================== Request Building Tests ====================

    #[test]
    fn test_gemini_request_serialization() {
        let request = GeminiRequest {
            contents: vec![GeminiContent {
                role: "user".to_string(),
                parts: vec![GeminiPart {
                    text: "Hello".to_string(),
                }],
            }],
            system_instruction: Some(GeminiSystemInstruction {
                parts: vec![GeminiPart {
                    text: "You are helpful.".to_string(),
                }],
            }),
            generation_config: Some(GeminiGenerationConfig {
                temperature: Some(0.7),
                top_p: Some(0.9),
                max_output_tokens: Some(1024),
                stop_sequences: Some(vec!["STOP".to_string()]),
            }),
        };

        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("\"contents\""));
        assert!(json.contains("\"systemInstruction\"")); // camelCase
        assert!(json.contains("\"generationConfig\"")); // camelCase
        assert!(json.contains("\"temperature\":0.7"));
        assert!(json.contains("\"maxOutputTokens\":1024")); // camelCase
    }

    #[test]
    fn test_gemini_request_skips_none_fields() {
        let request = GeminiRequest {
            contents: vec![],
            system_instruction: None,
            generation_config: None,
        };

        let json = serde_json::to_string(&request).unwrap();

        assert!(!json.contains("\"systemInstruction\""));
        assert!(!json.contains("\"generationConfig\""));
    }

    #[test]
    fn test_gemini_generation_config_skips_none_fields() {
        let config = GeminiGenerationConfig {
            temperature: Some(0.5),
            top_p: None,
            max_output_tokens: None,
            stop_sequences: None,
        };

        let json = serde_json::to_string(&config).unwrap();

        assert!(json.contains("\"temperature\":0.5"));
        assert!(!json.contains("\"topP\""));
        assert!(!json.contains("\"maxOutputTokens\""));
        assert!(!json.contains("\"stopSequences\""));
    }

    // ==================== Usage Metadata Tests ====================

    #[test]
    fn test_usage_metadata_defaults() {
        let json = r#"{}"#;

        let usage: GeminiUsageMetadata = serde_json::from_str(json).unwrap();

        assert_eq!(usage.prompt_token_count, 0);
        assert_eq!(usage.candidates_token_count, 0);
        assert_eq!(usage.total_token_count, 0);
    }

    #[test]
    fn test_usage_metadata_partial() {
        let json = r#"{"promptTokenCount": 10}"#;

        let usage: GeminiUsageMetadata = serde_json::from_str(json).unwrap();

        assert_eq!(usage.prompt_token_count, 10);
        assert_eq!(usage.candidates_token_count, 0);
        assert_eq!(usage.total_token_count, 0);
    }

    // ==================== URL Building Tests ====================

    #[test]
    fn test_streaming_url_format() {
        let base_url = "https://generativelanguage.googleapis.com/v1beta";
        let model = "gemini-1.5-pro";

        // API key is passed via x-goog-api-key header, not in URL
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            base_url, model
        );

        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn test_non_streaming_url_format() {
        let base_url = "https://generativelanguage.googleapis.com/v1beta";
        let model = "gemini-1.5-pro";

        // API key is passed via x-goog-api-key header, not in URL
        let url = format!("{}/models/{}:generateContent", base_url, model);

        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:generateContent"
        );
    }

    #[test]
    fn test_api_key_header() {
        // Verify that the x-goog-api-key header can be created
        let api_key = "test-api-key-123";
        let header_value = reqwest::header::HeaderValue::from_str(api_key);
        assert!(header_value.is_ok());
        assert_eq!(header_value.unwrap().to_str().unwrap(), api_key);
    }

    // ==================== Content Structure Tests ====================

    #[test]
    fn test_gemini_content_serialization() {
        let content = GeminiContent {
            role: "user".to_string(),
            parts: vec![GeminiPart {
                text: "Hello world".to_string(),
            }],
        };

        let json = serde_json::to_string(&content).unwrap();

        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"text\":\"Hello world\""));
    }

    #[test]
    fn test_gemini_system_instruction_serialization() {
        let instruction = GeminiSystemInstruction {
            parts: vec![GeminiPart {
                text: "Be helpful".to_string(),
            }],
        };

        let json = serde_json::to_string(&instruction).unwrap();

        assert!(json.contains("\"parts\""));
        assert!(json.contains("\"text\":\"Be helpful\""));
    }

    // ==================== SSE Parser Tests ====================

    #[tokio::test]
    async fn test_sse_parser_multiple_events_in_single_packet() {
        use futures_util::StreamExt;

        // Simulate multiple SSE events arriving in a single network packet
        // This tests that the parser doesn't lose events when process_buffer() returns multiple results
        let multi_event_packet = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"},\"finishReason\":null,\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1,\"totalTokenCount\":11}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" World\"}],\"role\":\"model\"},\"finishReason\":null,\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":2,\"totalTokenCount\":12}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":3,\"totalTokenCount\":13}}\n\n",
        );

        // Create a mock stream that returns all events in one packet
        let bytes = bytes::Bytes::from(multi_event_packet);
        let mock_stream = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes)]);

        let parser = new_gemini_sse_parser(mock_stream, "gemini-1.5-pro".to_string());
        let events: Vec<_> = parser.collect().await;

        // Should have received all 3 events
        assert_eq!(events.len(), 3, "Expected 3 events, got {}", events.len());

        // Verify each event is Ok
        for (i, event) in events.iter().enumerate() {
            assert!(event.is_ok(), "Event {} should be Ok", i);
        }
    }

    #[tokio::test]
    async fn test_sse_parser_events_split_across_packets() {
        use futures_util::StreamExt;

        // Test events split across multiple network packets
        let packet1 = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"},\"finishReason\":null,\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1,\"totalTokenCount\":11}}\n\n";
        let packet2 = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" World\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":2,\"totalTokenCount\":12}}\n\n";

        let mock_stream = futures_util::stream::iter(vec![
            Ok::<_, reqwest::Error>(bytes::Bytes::from(packet1)),
            Ok(bytes::Bytes::from(packet2)),
        ]);

        let parser = new_gemini_sse_parser(mock_stream, "gemini-1.5-pro".to_string());
        let events: Vec<_> = parser.collect().await;

        assert_eq!(events.len(), 2, "Expected 2 events, got {}", events.len());

        for event in &events {
            assert!(event.is_ok());
        }
    }

    // ==================== ID Uniqueness Tests ====================

    #[tokio::test]
    async fn test_gemini_ids_are_unique_across_concurrent_requests() {
        use futures_util::StreamExt;
        use std::collections::HashSet;

        // Create multiple parsers simultaneously (simulating concurrent requests)
        // All should have unique IDs even if created within the same second
        let mut ids = HashSet::new();
        let num_requests = 100;

        let sample_response = r#"{"candidates":[{"content":{"parts":[{"text":"Hi"}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1,"totalTokenCount":11}}"#;

        for _ in 0..num_requests {
            let packet = format!("data: {}\n\n", sample_response);
            let mock_stream = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(
                bytes::Bytes::from(packet),
            )]);
            let parser = new_gemini_sse_parser(mock_stream, "gemini-1.5-pro".to_string());

            // Get the first event to extract the request ID
            let events: Vec<_> = parser.collect().await;
            assert!(!events.is_empty(), "Should get at least one event");

            let request_id = events
                .into_iter()
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    if let StreamChunk::Chat(chunk) = e.chunk {
                        Some(chunk.id)
                    } else {
                        None
                    }
                })
                .next()
                .expect("Should have a chunk with ID");

            // Verify ID has correct format (gemini-<uuid>)
            assert!(
                request_id.starts_with("gemini-"),
                "ID should start with 'gemini-'"
            );
            assert!(
                request_id.len() > 7,
                "ID should have UUID component after prefix"
            );

            // Check for uniqueness
            let is_unique = ids.insert(request_id.clone());
            assert!(
                is_unique,
                "ID should be unique, but got duplicate: {}",
                request_id
            );
        }

        assert_eq!(
            ids.len(),
            num_requests,
            "All {} IDs should be unique",
            num_requests
        );
    }

    #[tokio::test]
    async fn test_gemini_streaming_response_has_consistent_id() {
        use futures_util::StreamExt;

        // Test that all chunks in a streaming response have the same ID
        let multi_event_packet = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"},\"finishReason\":null,\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1,\"totalTokenCount\":11}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" World\"}],\"role\":\"model\"},\"finishReason\":null,\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":2,\"totalTokenCount\":12}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":3,\"totalTokenCount\":13}}\n\n",
        );

        let bytes = bytes::Bytes::from(multi_event_packet);
        let mock_stream = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes)]);

        let parser = new_gemini_sse_parser(mock_stream, "gemini-1.5-pro".to_string());
        let events: Vec<_> = parser.collect().await;

        // Extract IDs from all chunks
        let mut ids: Vec<String> = Vec::new();
        for sse_event in events.into_iter().flatten() {
            if let StreamChunk::Chat(chat_chunk) = sse_event.chunk {
                ids.push(chat_chunk.id.clone());
            }
        }

        assert!(!ids.is_empty(), "Should have collected chunk IDs");

        // All IDs should be the same within a single request
        let first_id = &ids[0];
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(
                id, first_id,
                "Chunk {} has different ID. Expected: {}, Got: {}",
                i, first_id, id
            );
        }

        // ID should have the correct format
        assert!(
            first_id.starts_with("gemini-"),
            "ID should start with 'gemini-'"
        );
    }

    // ==================== Vendor Prefix Stripping Tests ====================

    #[test]
    fn test_strip_vendor_prefix_google() {
        assert_eq!(
            strip_vendor_prefix("google/gemini-2.0-flash"),
            "gemini-2.0-flash"
        );
        assert_eq!(
            strip_vendor_prefix("google/gemini-1.5-pro"),
            "gemini-1.5-pro"
        );
    }

    #[test]
    fn test_strip_vendor_prefix_vertex() {
        assert_eq!(
            strip_vendor_prefix("vertex/gemini-2.0-flash"),
            "gemini-2.0-flash"
        );
    }

    #[test]
    fn test_strip_vendor_prefix_no_prefix() {
        assert_eq!(strip_vendor_prefix("gemini-2.0-flash"), "gemini-2.0-flash");
        assert_eq!(
            strip_vendor_prefix("imagen-3.0-generate-001"),
            "imagen-3.0-generate-001"
        );
    }

    #[test]
    fn test_strip_vendor_prefix_unknown_prefix() {
        // Unknown prefixes should be left as-is
        assert_eq!(
            strip_vendor_prefix("unknown/gemini-2.0-flash"),
            "unknown/gemini-2.0-flash"
        );
    }

    #[test]
    fn test_url_construction_with_prefixed_model() {
        let base_url = "https://generativelanguage.googleapis.com/v1beta";
        let model = "google/gemini-2.0-flash";
        let model_name = strip_vendor_prefix(model);
        let url = format!("{}/models/{}:generateContent", base_url, model_name);

        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent"
        );
    }

    // ==================== Image Generation Tests ====================

    #[test]
    fn test_image_generation_url() {
        let base_url = "https://generativelanguage.googleapis.com/v1beta";
        let model = "imagen-3.0-generate-001";
        let url = format!("{}/models/{}:generateImages", base_url, model);

        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/imagen-3.0-generate-001:generateImages"
        );
    }

    #[test]
    fn test_size_to_aspect_ratio_square() {
        assert_eq!(
            size_to_aspect_ratio(Some(&"1024x1024".to_string())),
            Some("1:1".to_string())
        );
        assert_eq!(
            size_to_aspect_ratio(Some(&"512x512".to_string())),
            Some("1:1".to_string())
        );
    }

    #[test]
    fn test_size_to_aspect_ratio_wide() {
        assert_eq!(
            size_to_aspect_ratio(Some(&"1920x1080".to_string())),
            Some("16:9".to_string())
        );
    }

    #[test]
    fn test_size_to_aspect_ratio_tall() {
        assert_eq!(
            size_to_aspect_ratio(Some(&"1080x1920".to_string())),
            Some("9:16".to_string())
        );
    }

    #[test]
    fn test_size_to_aspect_ratio_none() {
        assert_eq!(size_to_aspect_ratio(None), None);
    }

    #[test]
    fn test_size_to_aspect_ratio_invalid() {
        assert_eq!(size_to_aspect_ratio(Some(&"invalid".to_string())), None);
        assert_eq!(size_to_aspect_ratio(Some(&"abc".to_string())), None);
    }

    #[test]
    fn test_size_to_aspect_ratio_unusual_size() {
        // Sizes that don't match any standard aspect ratio
        // Using a ratio far from any standard (e.g., 2:1 = 2.0)
        assert_eq!(size_to_aspect_ratio(Some(&"2000x1000".to_string())), None);
    }

    #[test]
    fn test_gemini_image_generation_request_serialization() {
        let request = GeminiImageGenerationRequest {
            prompt: "A cat wearing a hat".to_string(),
            number_of_images: Some(2),
            aspect_ratio: Some("1:1".to_string()),
            safety_filter_level: None,
            person_generation: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"prompt\":\"A cat wearing a hat\""));
        assert!(json.contains("\"numberOfImages\":2"));
        assert!(json.contains("\"aspectRatio\":\"1:1\""));
        // These should not be serialized when None
        assert!(!json.contains("safetyFilterLevel"));
        assert!(!json.contains("personGeneration"));
    }

    #[test]
    fn test_gemini_image_generation_response_deserialization() {
        let json = r#"{
            "generatedImages": [
                {"image": {"imageBytes": "base64encodedimage1"}},
                {"image": {"imageBytes": "base64encodedimage2"}}
            ]
        }"#;

        let response: GeminiImageGenerationResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.generated_images.len(), 2);
        assert_eq!(
            response.generated_images[0].image.image_bytes,
            "base64encodedimage1"
        );
        assert_eq!(
            response.generated_images[1].image.image_bytes,
            "base64encodedimage2"
        );
    }

    #[test]
    fn test_gemini_image_generation_response_empty() {
        let json = r#"{"generatedImages": []}"#;

        let response: GeminiImageGenerationResponse = serde_json::from_str(json).unwrap();
        assert!(response.generated_images.is_empty());
    }
}
