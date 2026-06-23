//! Anthropic backend implementation
//!
//! This backend handles HTTP communication with Anthropic's Messages API.
//! Format conversion is handled by the `anthropic_converter` module.

pub mod converter;

use super::backend::{BackendConfig, ExternalBackend};
use crate::{
    BufferedSSEParser, ChatCompletionParams, ChatCompletionResponse, ChatCompletionResponseChoice,
    ChatCompletionResponseWithBytes, ChatResponseMessage, CompletionError, MessageRole,
    StreamingResult, TokenUsage,
};
use async_trait::async_trait;
use bytes::Bytes;
use converter::{
    convert_messages, convert_tool_choice, convert_tools, extract_response_content,
    map_finish_reason_string, AnthropicEventParser, AnthropicParserState, AnthropicRequest,
    AnthropicResponse, AnthropicUsage,
};
use futures_util::Stream;
use reqwest::{header::HeaderValue, Client};

const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Caller-supplied `extra` keys we forward to Anthropic's Messages API.
///
/// This is an allowlist on purpose: `ChatCompletionParams.extra` is an
/// unbounded catch-all that also holds internal E2EE keys and OpenAI-only
/// fields, so we only pass through the reasoning controls Anthropic actually
/// understands (`thinking`) plus `reasoning_effort` (which Anthropic does not
/// accept and will reject with its own 400, instead of us silently dropping it).
const ANTHROPIC_PASSTHROUGH_KEYS: &[&str] = &["thinking", "reasoning_effort"];

/// Anthropic model-name fragments that **reject any non-default `temperature`**
/// with a 400 (`temperature is deprecated for this model`), even though they
/// still advertise `temperature` (nearai/cloud-api #696).
///
/// These are matched as substrings so both the bare alias (`claude-opus-4-7`)
/// and the dated form (`claude-opus-4-7-20XXYYZZ`) are covered. opus-4-6 and
/// earlier still accept `temperature`, so they are intentionally absent — do
/// not over-strip.
const ANTHROPIC_MODELS_REJECTING_TEMPERATURE: &[&str] = &["claude-opus-4-7"];

/// Whether `model` rejects a non-default `temperature` (and also `top_p`), so we
/// must drop BOTH sampling knobs rather than 400 the caller (#696). opus-4-7
/// returns `temperature is deprecated` / `top_p is deprecated` for either.
fn rejects_non_default_temperature(model: &str) -> bool {
    ANTHROPIC_MODELS_REJECTING_TEMPERATURE
        .iter()
        .any(|fragment| model.contains(fragment))
}

/// Whether a requested `response_format` asks for JSON output, which Anthropic
/// has no native mode for and tends to return markdown-fenced (#668). When
/// true, we strip code fences from the response so `JSON.parse` works.
fn wants_json_output(extra: &std::collections::HashMap<String, serde_json::Value>) -> bool {
    extra
        .get("response_format")
        .and_then(|rf| rf.get("type"))
        .and_then(|t| t.as_str())
        .map(|t| t == "json_object" || t == "json_schema")
        .unwrap_or(false)
}

/// Strip a single leading/trailing markdown code fence from `content`.
///
/// Anthropic has no native JSON-output mode, so when a caller requests
/// `response_format: json_object`/`json_schema` the model frequently wraps the
/// JSON in a ` ```json … ``` ` block. This unwraps that fence so the content is
/// raw parseable JSON (#668). Content without a fence is returned unchanged.
fn strip_json_code_fence(content: &str) -> String {
    let trimmed = content.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return content.to_string();
    };
    // Drop the optional language tag on the opening fence line (e.g. `json`).
    let after_lang = match rest.find('\n') {
        Some(idx) => &rest[idx + 1..],
        None => return content.to_string(),
    };
    let Some(inner) = after_lang.trim_end().strip_suffix("```") else {
        return content.to_string();
    };
    inner.trim().to_string()
}

/// Map a non-streaming Anthropic `usage` block to OpenAI-shaped [`TokenUsage`].
///
/// #666: surface Anthropic prompt-cache stats so the existing billing path
/// (which reads `usage.cached_tokens()` and bills `cache_read_tokens`) lights
/// up. CRITICAL accounting: Anthropic reports cache reads and cache creation
/// SEPARATELY from `input_tokens`, whereas OpenAI's `cached_tokens` is a SUBSET
/// of `prompt_tokens` (and `TokenUsage::cached_tokens()` caps it to
/// `[0, prompt_tokens]`). To preserve that invariant AND bill the cache-read
/// cost, we ADD both cache figures into `prompt_tokens` and report the read
/// portion as `cached_tokens`. When there is no cache read, `prompt_tokens_details`
/// stays `None` so an uncached response is byte-identical to before.
fn map_usage(usage: &AnthropicUsage) -> TokenUsage {
    let prompt_tokens =
        usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
    let prompt_tokens_details = if usage.cache_read_input_tokens > 0 {
        Some(serde_json::json!({ "cached_tokens": usage.cache_read_input_tokens }))
    } else {
        None
    };
    TokenUsage {
        prompt_tokens,
        completion_tokens: usage.output_tokens,
        total_tokens: prompt_tokens + usage.output_tokens,
        prompt_tokens_details,
    }
}

/// Pick the allowlisted reasoning-control fields out of `extra`.
fn extract_passthrough(
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> std::collections::HashMap<String, serde_json::Value> {
    ANTHROPIC_PASSTHROUGH_KEYS
        .iter()
        .filter_map(|&key| extra.get(key).map(|value| (key.to_string(), value.clone())))
        .collect()
}

/// Build our normalized OpenAI-shaped response from a parsed Anthropic
/// non-streaming response.
///
/// `sent_model` is the model name cloud-api sent to Anthropic (the `model`
/// argument threaded through the backend call), NOT `anthropic_response.model`.
///
/// #632: the response `model` field uses the requested/sent model name to stay
/// consistent with the streaming path, which seeds its chunk model from the
/// same sent name (see `AnthropicParserState::new` in `converter.rs`).
/// Upstream's dated canonical name (e.g. `claude-haiku-4-5-20251001`) is
/// intentionally NOT surfaced, so both transports echo the same value for an
/// identical request.
fn build_openai_response(
    anthropic_response: AnthropicResponse,
    sent_model: &str,
    wants_json: bool,
) -> ChatCompletionResponse {
    // Convert to OpenAI format using the converter
    let (content, tool_calls) = extract_response_content(&anthropic_response.content);

    // #668: Anthropic has no native JSON-output mode, so when the caller
    // requested `response_format: json_object`/`json_schema` it tends to
    // wrap the JSON in a markdown ```json … ``` fence, breaking
    // `JSON.parse`. Strip that fence so the content is raw parseable JSON.
    let content = if wants_json {
        content.map(|c| strip_json_code_fence(&c))
    } else {
        content
    };

    ChatCompletionResponse {
        id: anthropic_response.id,
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp(),
        // #632: echo the requested/sent model name (not upstream's dated name)
        // so non-streaming matches the streaming path for the same request.
        model: sent_model.to_string(),
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
            finish_reason: map_finish_reason_string(anthropic_response.stop_reason),
            token_ids: None,
            extra: Default::default(),
        }],
        service_tier: None,
        system_fingerprint: None,
        // #666: fold Anthropic cache-read/creation tokens into prompt_tokens and
        // surface the read portion as prompt_tokens_details.cached_tokens.
        usage: map_usage(&anthropic_response.usage),
        prompt_logprobs: None,
        prompt_token_ids: None,
        kv_transfer_params: None,
        extra: Default::default(),
    }
}

/// Anthropic backend - handles HTTP communication with Anthropic's API
pub struct AnthropicBackend {
    client: Client,
}

impl AnthropicBackend {
    pub fn new() -> Self {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .expect("Failed to create HTTP client");

        Self { client }
    }

    fn build_headers(&self, config: &BackendConfig) -> Result<reqwest::header::HeaderMap, String> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        let header_value = HeaderValue::from_str(&config.api_key)
            .map_err(|e| format!("Invalid API key format: {e}"))?;
        headers.insert("x-api-key", header_value);

        let version = config
            .extra
            .get("version")
            .map(|s| s.as_str())
            .unwrap_or(DEFAULT_ANTHROPIC_VERSION);
        if let Ok(value) = HeaderValue::from_str(version) {
            headers.insert("anthropic-version", value);
        }

        Ok(headers)
    }

    fn build_request(
        &self,
        model: &str,
        params: &ChatCompletionParams,
        stream: bool,
    ) -> AnthropicRequest {
        let (system, messages) = convert_messages(&params.messages);
        let max_tokens = params
            .max_completion_tokens
            .or(params.max_tokens)
            .unwrap_or(4096);

        // Convert tools if provided
        let tools = params.tools.as_ref().map(|t| convert_tools(t));
        let tool_choice = params.tool_choice.as_ref().and_then(convert_tool_choice);

        // Anthropic doesn't allow both temperature and top_p - prefer temperature if both are set.
        // Also clamp temperature to Anthropic's valid range [0.0, 1.0] (OpenAI allows up to 2.0).
        //
        // #696: some newer models (e.g. claude-opus-4-7) 400 on ANY non-default
        // `temperature` — AND on any `top_p` ("`top_p` is deprecated for this
        // model"). So we drop BOTH and forward neither, letting the model use
        // its own defaults; OpenAI/OpenRouter clients that routinely send
        // `temperature: 0`/`0.7` (and our own `top_p` default of 1.0) then get a
        // 200 with the params ignored instead of a 400. NOTE: `top_p` defaults to
        // `Some(1.0)` at deserialization, so forwarding `params.top_p` here would
        // send `top_p: 1.0` unconditionally and 400 every request — we must send
        // `None` for both.
        let (temperature, top_p) = if rejects_non_default_temperature(model) {
            (None, None)
        } else if let Some(temp) = params.temperature {
            (Some(temp.clamp(0.0, 1.0)), None)
        } else {
            (None, params.top_p)
        };

        AnthropicRequest {
            model: model.to_string(),
            messages,
            max_tokens,
            system,
            temperature,
            top_p,
            stop_sequences: params.stop.clone(),
            tools,
            tool_choice,
            stream,
            // Forward only the reasoning-control fields from `extra`, not the
            // whole map. A full passthrough is unsafe here: `extra` also carries
            // internal E2EE keys (`x_signing_algo`, `x_client_pub_key`, …) that
            // must never reach Anthropic, OpenAI-only fields that Anthropic
            // rejects (`max_completion_tokens`, `presence_penalty`,
            // `frequency_penalty`, …), and could collide with named fields
            // (`system`, `stop_sequences`) producing duplicate JSON keys.
            extra: extract_passthrough(&params.extra),
        }
    }
}

impl Default for AnthropicBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// SSE parser type alias for Anthropic
pub type AnthropicSSEParser<S> = BufferedSSEParser<S, AnthropicEventParser>;

/// Create a new Anthropic SSE parser
pub fn new_anthropic_sse_parser<S>(stream: S, model: String) -> AnthropicSSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    BufferedSSEParser::new(stream, AnthropicParserState::new(model))
}

#[async_trait]
impl ExternalBackend for AnthropicBackend {
    fn backend_type(&self) -> &'static str {
        "anthropic"
    }

    async fn chat_completion_stream(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        // NOTE (#668): markdown-fence stripping for `response_format` JSON modes
        // is applied on the non-streaming path only. A fence marker (```) can
        // split across SSE deltas, so reliably stripping it mid-stream would
        // require buffering the whole response and defeats streaming. The
        // model's own behaviour with the json hint is usually fence-free when
        // streaming; callers needing guaranteed raw JSON should use the
        // non-streaming endpoint.
        let url = format!("{}/messages", config.base_url);
        let request = self.build_request(model, &params, true);

        let headers = self
            .build_headers(config)
            .map_err(CompletionError::CompletionError)?;
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

        let sse_stream = new_anthropic_sse_parser(response.bytes_stream(), model.to_string());
        Ok(Box::pin(sse_stream))
    }

    async fn chat_completion(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        let url = format!("{}/messages", config.base_url);
        let request = self.build_request(model, &params, false);

        let headers = self
            .build_headers(config)
            .map_err(CompletionError::CompletionError)?;
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

        let anthropic_response: AnthropicResponse =
            serde_json::from_slice(&raw_bytes).map_err(|e| {
                CompletionError::CompletionError(format!("Failed to parse response: {e}"))
            })?;

        let openai_response =
            build_openai_response(anthropic_response, model, wants_json_output(&params.extra));

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
            serving_tier: crate::ProviderTier::NonAttested,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_headers_default_version() {
        let backend = AnthropicBackend::new();
        let config = BackendConfig {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "test-key".to_string(),
            timeout_seconds: 30,
            extra: std::collections::HashMap::new(),
            extra_request_body: std::collections::HashMap::new(),
        };

        let headers = backend.build_headers(&config).unwrap();

        assert_eq!(
            headers.get("x-api-key").unwrap().to_str().unwrap(),
            "test-key"
        );
        assert_eq!(
            headers.get("anthropic-version").unwrap().to_str().unwrap(),
            DEFAULT_ANTHROPIC_VERSION
        );
    }

    #[test]
    fn test_build_headers_custom_version() {
        let backend = AnthropicBackend::new();
        let mut extra = std::collections::HashMap::new();
        extra.insert("version".to_string(), "2024-01-01".to_string());

        let config = BackendConfig {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "test-key".to_string(),
            timeout_seconds: 30,
            extra,
            extra_request_body: std::collections::HashMap::new(),
        };

        let headers = backend.build_headers(&config).unwrap();

        assert_eq!(
            headers.get("anthropic-version").unwrap().to_str().unwrap(),
            "2024-01-01"
        );
    }

    fn make_params(temperature: Option<f32>, top_p: Option<f32>) -> ChatCompletionParams {
        ChatCompletionParams {
            model: "claude-sonnet-4-5-20250514".to_string(),
            messages: vec![crate::ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::Value::String("Hello".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_completion_tokens: None,
            max_tokens: None,
            temperature,
            top_p,
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
            modalities: None,
            extra: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_build_request_temperature_only() {
        let backend = AnthropicBackend::new();
        let params = make_params(Some(0.7), None);
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        assert_eq!(request.temperature, Some(0.7));
        assert_eq!(request.top_p, None);
    }

    #[test]
    fn test_build_request_top_p_only() {
        let backend = AnthropicBackend::new();
        let params = make_params(None, Some(0.9));
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        assert_eq!(request.temperature, None);
        assert_eq!(request.top_p, Some(0.9));
    }

    #[test]
    fn test_build_request_both_temperature_and_top_p_prefers_temperature() {
        let backend = AnthropicBackend::new();
        let params = make_params(Some(0.5), Some(0.9));
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        // Anthropic doesn't allow both; temperature takes precedence
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.top_p, None);
    }

    #[test]
    fn test_build_request_neither_temperature_nor_top_p() {
        let backend = AnthropicBackend::new();
        let params = make_params(None, None);
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        assert_eq!(request.temperature, None);
        assert_eq!(request.top_p, None);
    }

    #[test]
    fn test_build_request_clamps_temperature_to_anthropic_range() {
        let backend = AnthropicBackend::new();
        // OpenAI allows temperature up to 2.0, Anthropic only allows up to 1.0
        let params = make_params(Some(1.5), None);
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        assert_eq!(request.temperature, Some(1.0));
    }

    #[test]
    fn test_build_request_default_max_tokens() {
        let backend = AnthropicBackend::new();
        let params = make_params(None, None);
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);

        assert_eq!(request.max_tokens, 4096);
    }

    #[test]
    fn test_build_request_forwards_thinking_config() {
        let backend = AnthropicBackend::new();
        let mut params = make_params(None, None);
        let thinking = serde_json::json!({"type": "enabled", "budget_tokens": 4096});
        params
            .extra
            .insert("thinking".to_string(), thinking.clone());

        let request = backend.build_request("claude-opus-4-7", &params, false);
        let body = serde_json::to_value(&request).unwrap();

        // The native Anthropic `thinking` object is forwarded verbatim as a
        // top-level request field so Anthropic applies extended thinking.
        assert_eq!(body.get("thinking"), Some(&thinking));
    }

    #[test]
    fn test_build_request_forwards_reasoning_effort() {
        let backend = AnthropicBackend::new();
        let mut params = make_params(None, None);
        params.extra.insert(
            "reasoning_effort".to_string(),
            serde_json::Value::String("high".to_string()),
        );

        let request = backend.build_request("claude-opus-4-7", &params, false);
        let body = serde_json::to_value(&request).unwrap();

        // We forward `reasoning_effort` rather than silently dropping it.
        // Anthropic validates the field and returns its own error if unsupported.
        assert_eq!(
            body.get("reasoning_effort"),
            Some(&serde_json::Value::String("high".to_string()))
        );
    }

    #[test]
    fn test_build_request_does_not_leak_openai_only_params() {
        let backend = AnthropicBackend::new();
        let mut params = make_params(None, None);
        // Typed OpenAI-only sampling params live in named struct fields, never
        // in `extra`, so they must not appear in the Anthropic request body.
        params.frequency_penalty = Some(0.5);
        params.presence_penalty = Some(0.5);

        let request = backend.build_request("claude-opus-4-7", &params, false);
        let body = serde_json::to_value(&request).unwrap();

        assert!(body.get("frequency_penalty").is_none());
        assert!(body.get("presence_penalty").is_none());
    }

    #[test]
    fn test_build_request_drops_non_allowlisted_extra_keys() {
        let backend = AnthropicBackend::new();
        let mut params = make_params(Some(1.0), None);
        params.stop = Some(vec!["STOP".to_string()]);
        // `extra` is an unbounded catch-all. None of these may reach Anthropic:
        // internal E2EE keys, OpenAI-only fields, or keys that collide with the
        // named request fields (`system`, `stop_sequences`).
        for key in [
            "x_signing_algo",
            "x_client_pub_key",
            "x_encryption_version",
            "x_encrypt_all_fields",
            "max_completion_tokens",
            "frequency_penalty",
            "presence_penalty",
            "response_format",
            "system",
            "stop_sequences",
        ] {
            params
                .extra
                .insert(key.to_string(), serde_json::json!("leak"));
        }

        let request = backend.build_request("claude-opus-4-7", &params, false);
        let obj = serde_json::to_value(&request).unwrap();
        let obj = obj.as_object().unwrap();

        // No internal/OpenAI-only key leaked through.
        for key in [
            "x_signing_algo",
            "x_client_pub_key",
            "x_encryption_version",
            "x_encrypt_all_fields",
            "max_completion_tokens",
            "frequency_penalty",
            "presence_penalty",
            "response_format",
        ] {
            assert!(obj.get(key).is_none(), "{key} must not be forwarded");
        }
        // Named fields keep their derived values, not the `extra` collision.
        assert!(obj.get("system").is_none()); // no system message -> field absent
        assert_eq!(
            obj.get("stop_sequences"),
            Some(&serde_json::json!(["STOP"])),
            "stop_sequences must come from params.stop, not extra"
        );
    }

    #[test]
    fn test_build_request_empty_extra_adds_no_fields() {
        let backend = AnthropicBackend::new();
        let params = make_params(Some(1.0), None);
        // Use a model that accepts temperature so this test isolates the
        // "extra adds nothing" property (opus-4-7 drops temperature, see #696).
        let request = backend.build_request("claude-sonnet-4-5-20250514", &params, false);
        let body = serde_json::to_value(&request).unwrap();

        // With no extra fields, the flattened `extra` map contributes nothing:
        // the serialized request carries only the known Anthropic fields.
        let keys: std::collections::HashSet<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        let expected: std::collections::HashSet<&str> =
            ["model", "messages", "max_tokens", "temperature", "stream"]
                .into_iter()
                .collect();
        assert_eq!(keys, expected);
    }

    // ── #696: temperature dropped for models that reject non-default values ──

    #[test]
    fn test_opus_4_7_drops_both_temperature_and_top_p() {
        let backend = AnthropicBackend::new();
        // opus-4-7 400s on any non-default `temperature` AND on any `top_p`
        // ("`top_p` is deprecated for this model"). Crucially `top_p` defaults to
        // Some(1.0) at deserialization, so forwarding it would 400 every request
        // — we must drop BOTH and let the model use its own defaults (#696).
        let params = make_params(Some(0.0), Some(0.5));
        let request = backend.build_request("claude-opus-4-7", &params, false);
        assert_eq!(
            request.temperature, None,
            "temperature must be dropped for opus-4-7"
        );
        assert_eq!(
            request.top_p, None,
            "top_p must also be dropped for opus-4-7 (it rejects top_p too)"
        );

        // Dated form + the defaulted top_p=1.0 (the real-world no-params case
        // that regressed): still send neither.
        let params = make_params(None, Some(1.0));
        let request = backend.build_request("claude-opus-4-7-20991231", &params, false);
        assert_eq!(request.temperature, None);
        assert_eq!(
            request.top_p, None,
            "the default top_p=1.0 must not be forwarded to opus-4-7"
        );
    }

    #[test]
    fn test_opus_4_6_still_accepts_temperature() {
        let backend = AnthropicBackend::new();
        // Regression guard against over-stripping: opus-4-6 still accepts it.
        let params = make_params(Some(0.5), None);
        let request = backend.build_request("claude-opus-4-6", &params, false);
        assert_eq!(
            request.temperature,
            Some(0.5),
            "opus-4-6 must still forward temperature"
        );
    }

    // ── #668: strip markdown code fences when json output was requested ──────

    fn json_format_extra(type_: &str) -> std::collections::HashMap<String, serde_json::Value> {
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "response_format".to_string(),
            serde_json::json!({"type": type_}),
        );
        extra
    }

    #[test]
    fn test_wants_json_output() {
        assert!(wants_json_output(&json_format_extra("json_object")));
        assert!(wants_json_output(&json_format_extra("json_schema")));
        assert!(!wants_json_output(&json_format_extra("text")));
        assert!(!wants_json_output(&std::collections::HashMap::new()));
    }

    #[test]
    fn test_strip_json_code_fence_unwraps_fenced_json() {
        let fenced = "```json\n{\n  \"city\": \"Paris\"\n}\n```";
        let stripped = strip_json_code_fence(fenced);
        assert_eq!(stripped, "{\n  \"city\": \"Paris\"\n}");
        // Result is valid parseable JSON.
        let v: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v["city"], "Paris");
    }

    #[test]
    fn test_strip_json_code_fence_handles_no_language_tag() {
        let fenced = "```\n{\"a\":1}\n```";
        assert_eq!(strip_json_code_fence(fenced), "{\"a\":1}");
    }

    #[test]
    fn test_strip_json_code_fence_passes_through_raw_json() {
        let raw = "{\"city\":\"Paris\"}";
        assert_eq!(strip_json_code_fence(raw), raw);
    }

    // ── #666: non-streaming usage maps cache reads -> cached_tokens ──────────

    #[test]
    fn test_map_usage_folds_cache_into_prompt_tokens() {
        // Anthropic reports cache reads/creation separately from input_tokens.
        // map_usage adds them into prompt_tokens and reports the read portion as
        // cached_tokens, so cached <= prompt holds and the cache read is billed.
        let usage = AnthropicUsage {
            input_tokens: 10,
            output_tokens: 30,
            cache_read_input_tokens: 80,
            cache_creation_input_tokens: 5,
        };
        let mapped = map_usage(&usage);

        // prompt_tokens = 10 + 80 + 5 = 95.
        assert_eq!(mapped.prompt_tokens, 95);
        assert_eq!(mapped.completion_tokens, 30);
        assert_eq!(mapped.total_tokens, 125);
        // cached_tokens surfaced via prompt_tokens_details, and clamped helper
        // confirms the invariant cached <= prompt.
        assert_eq!(mapped.cached_tokens(), 80);
        assert!(mapped.cached_tokens() <= mapped.prompt_tokens);
    }

    #[test]
    fn test_map_usage_no_cache_omits_details() {
        // No cache reads -> prompt_tokens_details stays None (no regression).
        let usage = AnthropicUsage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        let mapped = map_usage(&usage);
        assert_eq!(mapped.prompt_tokens, 100);
        assert_eq!(mapped.total_tokens, 120);
        assert!(mapped.prompt_tokens_details.is_none());
        assert_eq!(mapped.cached_tokens(), 0);
    }

    // ── #632: non-streaming response echoes the SENT model name ─────────────

    /// Build a minimal Anthropic non-streaming response whose `model` field is
    /// the upstream dated canonical name, as Anthropic actually returns.
    fn make_anthropic_response(upstream_dated_model: &str) -> AnthropicResponse {
        serde_json::from_value(serde_json::json!({
            "id": "msg_test_632",
            "model": upstream_dated_model,
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": "Hello" }],
            "usage": { "input_tokens": 3, "output_tokens": 1 }
        }))
        .expect("test AnthropicResponse should deserialize")
    }

    #[test]
    fn test_non_streaming_response_model_is_sent_name_not_upstream_dated() {
        // For the SAME request, streaming echoes the SENT name (the `model` arg
        // threaded into the backend, seeded into AnthropicParserState), while
        // Anthropic's non-streaming JSON carries the UPSTREAM dated name. Plan A
        // for #632 makes non-streaming match streaming: the response `model`
        // must be the sent name, not the upstream dated name.
        let sent = "claude-haiku-4-5";
        let upstream_dated = "claude-haiku-4-5-20251001";

        let anthropic_response = make_anthropic_response(upstream_dated);
        // Sanity: the parsed upstream payload really carries the dated name, so
        // this test would catch a regression that surfaces it.
        assert_eq!(anthropic_response.model, upstream_dated);

        let openai_response = build_openai_response(anthropic_response, sent, false);

        assert_eq!(
            openai_response.model, sent,
            "non-streaming response must echo the SENT model name (transport consistency, #632)"
        );
        assert_ne!(
            openai_response.model, upstream_dated,
            "non-streaming response must NOT surface upstream's dated canonical name"
        );
    }

    #[test]
    fn test_non_streaming_response_model_matches_streaming_seed() {
        // The streaming path seeds its per-chunk `model` from the same sent name
        // via AnthropicParserState::new(model). Confirm the non-streaming helper
        // and the streaming parser state agree on the model echoed for an
        // identical request, regardless of what upstream returned (#632).
        let sent = "claude-sonnet-4-5";
        let upstream_dated = "claude-sonnet-4-5-20250929";

        let non_streaming =
            build_openai_response(make_anthropic_response(upstream_dated), sent, false);
        let streaming_state = AnthropicParserState::new(sent.to_string());

        assert_eq!(non_streaming.model, streaming_state.model);
        assert_eq!(non_streaming.model, sent);
    }

    #[tokio::test]
    async fn test_image_generation_returns_error() {
        let backend = AnthropicBackend::new();
        let config = BackendConfig {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "test-key".to_string(),
            timeout_seconds: 30,
            extra: std::collections::HashMap::new(),
            extra_request_body: std::collections::HashMap::new(),
        };

        let params = crate::ImageGenerationParams {
            model: "claude-3-opus".to_string(),
            prompt: "A cat".to_string(),
            n: None,
            size: None,
            response_format: None,
            quality: None,
            style: None,
            extra: std::collections::HashMap::new(),
        };

        let result = backend
            .image_generation(&config, "claude-3-opus", params)
            .await;

        assert!(result.is_err());
    }
}
