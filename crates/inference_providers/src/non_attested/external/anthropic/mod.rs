//! Anthropic backend implementation
//!
//! This backend handles HTTP communication with Anthropic's Messages API.
//! Format conversion is handled by the `anthropic_converter` module.

pub mod converter;

use super::backend::{BackendConfig, ExternalBackend};
#[cfg(test)]
use crate::MessageRole;
use crate::{
    AnthropicRawError, AnthropicRawRequest, AnthropicRawResponse, BufferedSSEParser,
    ChatCompletionParams, ChatCompletionResponse, ChatCompletionResponseWithBytes, CompletionError,
    StreamingResult,
};
use async_trait::async_trait;
use bytes::Bytes;
use converter::{
    convert_messages, convert_tool_choice, convert_tools, AnthropicEventParser,
    AnthropicParserState, AnthropicRequest,
};
use futures_util::{Stream, StreamExt as _};
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

/// Pick the allowlisted reasoning-control fields out of `extra`.
fn extract_passthrough(
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> std::collections::HashMap<String, serde_json::Value> {
    ANTHROPIC_PASSTHROUGH_KEYS
        .iter()
        .filter_map(|&key| extra.get(key).map(|value| (key.to_string(), value.clone())))
        .collect()
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

    fn build_raw_headers(
        &self,
        config: &BackendConfig,
        request: &AnthropicRawRequest,
    ) -> Result<reqwest::header::HeaderMap, AnthropicRawError> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let api_key = HeaderValue::from_str(&config.api_key).map_err(|error| {
            AnthropicRawError::Transport(format!("invalid upstream credential: {error}"))
        })?;
        headers.insert("x-api-key", api_key);

        let version = request
            .headers
            .version
            .as_deref()
            .or_else(|| config.extra.get("version").map(String::as_str))
            .unwrap_or(DEFAULT_ANTHROPIC_VERSION);
        let version = HeaderValue::from_str(version).map_err(|error| {
            AnthropicRawError::InvalidRequest(format!("invalid anthropic-version header: {error}"))
        })?;
        headers.insert("anthropic-version", version);

        if let Some(beta) = request.headers.beta.as_deref() {
            let beta = HeaderValue::from_str(beta).map_err(|error| {
                AnthropicRawError::InvalidRequest(format!("invalid anthropic-beta header: {error}"))
            })?;
            headers.insert("anthropic-beta", beta);
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

    /// Build the Messages body from the original Chat Completions JSON when it
    /// is available. Requests synthesized by internal callers (for example the
    /// Responses API) retain the legacy typed conversion during migration.
    fn build_request_body(
        &self,
        model: &str,
        params: &ChatCompletionParams,
        stream: bool,
    ) -> Result<serde_json::Value, CompletionError> {
        if let Some(original) = params.original_request.as_ref() {
            let converted = anthropic_compat::convert_openai_request(
                original,
                &anthropic_compat::ConvertOptions {
                    model: model.to_string(),
                    stream,
                },
            )
            .map_err(|error| CompletionError::HttpError {
                status_code: 400,
                message: error.message,
                is_external: false,
            })?;

            if !converted.warnings.is_empty() {
                let parameters = converted
                    .warnings
                    .iter()
                    .map(|warning| warning.parameter.as_str())
                    .collect::<Vec<_>>();
                tracing::debug!(
                    model,
                    ?parameters,
                    "Anthropic compatibility adapter omitted parameters"
                );
            }
            return Ok(converted.body);
        }

        serde_json::to_value(self.build_request(model, params, stream)).map_err(|error| {
            CompletionError::CompletionError(format!(
                "Failed to serialize Anthropic request: {error}"
            ))
        })
    }
}

impl Default for AnthropicBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn map_raw_transport_error(error: AnthropicRawError) -> CompletionError {
    match error {
        AnthropicRawError::InvalidRequest(message) => CompletionError::HttpError {
            status_code: 400,
            message,
            is_external: false,
        },
        AnthropicRawError::UnsupportedProvider => CompletionError::CompletionError(
            "Anthropic Messages transport is unavailable".to_string(),
        ),
        AnthropicRawError::Transport(message) => CompletionError::CompletionError(message),
    }
}

async fn collect_raw_body(
    response: &mut AnthropicRawResponse,
) -> Result<Vec<u8>, AnthropicRawError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.body.next().await {
        body.extend_from_slice(&chunk?);
    }
    Ok(body)
}

/// SSE parser type alias for Anthropic
pub type AnthropicSSEParser<S, E = reqwest::Error> = BufferedSSEParser<S, AnthropicEventParser, E>;

/// Create a new Anthropic SSE parser
pub fn new_anthropic_sse_parser<S, E>(stream: S, model: String) -> AnthropicSSEParser<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
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
        let request = self.build_request_body(model, &params, true)?;
        let mut response = self
            .anthropic_raw(
                config,
                model,
                AnthropicRawRequest {
                    endpoint: crate::AnthropicRawEndpoint::Messages,
                    beta: false,
                    body: request,
                    headers: crate::AnthropicRawHeaders::default(),
                },
            )
            .await
            .map_err(map_raw_transport_error)?;

        if !response.status.is_success() {
            let status_code = response.status.as_u16();
            let error_text = collect_raw_body(&mut response)
                .await
                .map(|body| String::from_utf8_lossy(&body).into_owned())
                .unwrap_or_else(|_| "Failed to read error response body".to_string());
            return Err(CompletionError::HttpError {
                status_code,
                message: crate::extract_error_message(&error_text),
                is_external: true,
            });
        }

        let sse_stream = new_anthropic_sse_parser(response.body, model.to_string());
        Ok(Box::pin(sse_stream))
    }

    async fn chat_completion(
        &self,
        config: &BackendConfig,
        model: &str,
        params: ChatCompletionParams,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        let request = self.build_request_body(model, &params, false)?;
        let mut response = self
            .anthropic_raw(
                config,
                model,
                AnthropicRawRequest {
                    endpoint: crate::AnthropicRawEndpoint::Messages,
                    beta: false,
                    body: request,
                    headers: crate::AnthropicRawHeaders::default(),
                },
            )
            .await
            .map_err(map_raw_transport_error)?;

        if !response.status.is_success() {
            let status_code = response.status.as_u16();
            let error_text = collect_raw_body(&mut response)
                .await
                .map(|body| String::from_utf8_lossy(&body).into_owned())
                .unwrap_or_else(|_| "Failed to read error response body".to_string());
            return Err(CompletionError::HttpError {
                status_code,
                message: crate::extract_error_message(&error_text),
                is_external: true,
            });
        }

        let raw_bytes = collect_raw_body(&mut response)
            .await
            .map_err(map_raw_transport_error)?;

        let anthropic_response: serde_json::Value =
            serde_json::from_slice(&raw_bytes).map_err(|e| {
                CompletionError::CompletionError(format!("Failed to parse response: {e}"))
            })?;
        let (converted, warnings) = anthropic_compat::convert_anthropic_response(
            &anthropic_response,
            &anthropic_compat::ResponseOptions {
                model: model.to_string(),
                created: chrono::Utc::now().timestamp(),
                strip_json_fence: wants_json_output(&params.extra),
            },
        )
        .map_err(|error| CompletionError::CompletionError(error.message))?;
        if !warnings.is_empty() {
            let parameters = warnings
                .iter()
                .map(|warning| warning.parameter.as_str())
                .collect::<Vec<_>>();
            tracing::debug!(
                model,
                ?parameters,
                "Anthropic compatibility adapter omitted response fields"
            );
        }
        let openai_response: ChatCompletionResponse =
            serde_json::from_value(converted).map_err(|e| {
                CompletionError::CompletionError(format!("Failed to convert response: {e}"))
            })?;

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

    async fn anthropic_raw(
        &self,
        config: &BackendConfig,
        model: &str,
        mut request: AnthropicRawRequest,
    ) -> Result<AnthropicRawResponse, AnthropicRawError> {
        let streaming = request.endpoint == crate::AnthropicRawEndpoint::Messages
            && request
                .body
                .get("stream")
                .and_then(serde_json::Value::as_bool)
                == Some(true);
        let body = request.body.as_object_mut().ok_or_else(|| {
            AnthropicRawError::InvalidRequest("request body must be a JSON object".to_string())
        })?;
        body.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );

        let mut url =
            reqwest::Url::parse(&format!("{}/{}", config.base_url, request.endpoint.path()))
                .map_err(|_| {
                    AnthropicRawError::Transport("invalid upstream base URL".to_string())
                })?;
        url.set_query(request.beta.then_some("beta=true"));
        let headers = self.build_raw_headers(config, &request)?;
        let timeout = std::time::Duration::from_secs(config.timeout_seconds as u64);

        let request_builder = self.client.post(url).headers(headers).json(&request.body);
        let response = if streaming {
            tokio::time::timeout(timeout, request_builder.send())
                .await
                .map_err(|_| {
                    AnthropicRawError::Transport(
                        "timed out waiting for Anthropic response headers".to_string(),
                    )
                })?
                .map_err(|error| AnthropicRawError::Transport(error.without_url().to_string()))?
        } else {
            request_builder
                .timeout(timeout)
                .send()
                .await
                .map_err(|error| AnthropicRawError::Transport(error.without_url().to_string()))?
        };

        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes_stream().map(|chunk| {
            chunk.map_err(|error| AnthropicRawError::Transport(error.without_url().to_string()))
        });

        Ok(AnthropicRawResponse {
            status,
            headers,
            body: Box::pin(body),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnthropicRawEndpoint, AnthropicRawHeaders};

    async fn collect_raw_body(mut response: AnthropicRawResponse) -> Vec<u8> {
        let mut body = Vec::new();
        while let Some(chunk) = response.body.next().await {
            body.extend_from_slice(&chunk.expect("raw response body chunk"));
        }
        body
    }

    fn raw_test_config(base_url: String) -> BackendConfig {
        let mut extra = std::collections::HashMap::new();
        extra.insert("version".to_string(), "2023-06-01".to_string());
        BackendConfig {
            base_url,
            api_key: "upstream-secret".to_string(),
            timeout_seconds: 30,
            extra,
            extra_request_body: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn raw_messages_rewrites_only_model_and_preserves_upstream_response() {
        use wiremock::matchers::{body_json, header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(query_param("beta", "true"))
            .and(header("x-api-key", "upstream-secret"))
            .and(header("anthropic-version", "2024-10-22"))
            .and(header("anthropic-beta", "prompt-caching-2024-07-31"))
            .and(body_json(serde_json::json!({
                "model": "claude-remote",
                "max_tokens": 17,
                "messages": [{"role": "user", "content": "hello"}],
                "future_field": {"nested": [1, 2, 3]}
            })))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "7")
                    .set_body_raw(
                        r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
                        "application/json",
                    ),
            )
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new();
        let response = backend
            .anthropic_raw(
                &raw_test_config(server.uri()),
                "claude-remote",
                AnthropicRawRequest {
                    endpoint: AnthropicRawEndpoint::Messages,
                    beta: true,
                    body: serde_json::json!({
                        "model": "anthropic/claude-cloud",
                        "max_tokens": 17,
                        "messages": [{"role": "user", "content": "hello"}],
                        "future_field": {"nested": [1, 2, 3]}
                    }),
                    headers: AnthropicRawHeaders {
                        version: Some("2024-10-22".to_string()),
                        beta: Some("prompt-caching-2024-07-31".to_string()),
                    },
                },
            )
            .await
            .expect("raw request should return the upstream response");

        assert_eq!(response.status, reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers.get("retry-after").unwrap(), "7");
        let body = collect_raw_body(response).await;
        assert_eq!(
            body,
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
        );
    }

    #[tokio::test]
    async fn raw_count_tokens_uses_separate_path_and_configured_version() {
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages/count_tokens"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(body_json(serde_json::json!({
                "model": "claude-remote",
                "messages": [{"role": "user", "content": "hello"}]
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"input_tokens":9}"#, "application/json"),
            )
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new();
        let response = backend
            .anthropic_raw(
                &raw_test_config(server.uri()),
                "claude-remote",
                AnthropicRawRequest {
                    endpoint: AnthropicRawEndpoint::CountTokens,
                    beta: false,
                    body: serde_json::json!({
                        "model": "claude-cloud",
                        "messages": [{"role": "user", "content": "hello"}]
                    }),
                    headers: AnthropicRawHeaders::default(),
                },
            )
            .await
            .expect("count_tokens passthrough should succeed");

        assert_eq!(response.status, reqwest::StatusCode::OK);
        assert_eq!(collect_raw_body(response).await, br#"{"input_tokens":9}"#);
    }

    #[tokio::test]
    async fn raw_request_rejects_non_object_body_before_network() {
        let backend = AnthropicBackend::new();
        let result = backend
            .anthropic_raw(
                &raw_test_config("http://127.0.0.1:1".to_string()),
                "claude-remote",
                AnthropicRawRequest {
                    endpoint: AnthropicRawEndpoint::Messages,
                    beta: false,
                    body: serde_json::json!(["not", "an", "object"]),
                    headers: AnthropicRawHeaders::default(),
                },
            )
            .await;

        let Err(error) = result else {
            panic!("array body must be rejected locally");
        };

        assert!(matches!(error, AnthropicRawError::InvalidRequest(_)));
    }

    #[test]
    fn test_build_raw_headers_default_version() {
        let backend = AnthropicBackend::new();
        let config = BackendConfig {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "test-key".to_string(),
            timeout_seconds: 30,
            extra: std::collections::HashMap::new(),
            extra_request_body: std::collections::HashMap::new(),
        };

        let headers = backend
            .build_raw_headers(
                &config,
                &AnthropicRawRequest {
                    endpoint: AnthropicRawEndpoint::Messages,
                    beta: false,
                    body: serde_json::json!({}),
                    headers: AnthropicRawHeaders::default(),
                },
            )
            .unwrap();

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
    fn test_build_raw_headers_custom_version() {
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

        let headers = backend
            .build_raw_headers(
                &config,
                &AnthropicRawRequest {
                    endpoint: AnthropicRawEndpoint::Messages,
                    beta: false,
                    body: serde_json::json!({}),
                    headers: AnthropicRawHeaders::default(),
                },
            )
            .unwrap();

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
            original_request: None,
            extra: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn original_wire_request_uses_anthropic_compat_adapter() {
        let backend = AnthropicBackend::new();
        let mut params = make_params(None, None);
        params.original_request = Some(serde_json::json!({
            "model": "anthropic/alias",
            "messages": [
                {"role": "system", "content": "first"},
                {"role": "developer", "content": "second"},
                {"role": "user", "content": "hello", "future_message_field": true}
            ],
            "max_completion_tokens": 37,
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "future_top_level": {"not": "forwarded"}
        }));

        let body = backend
            .build_request_body("claude-upstream", &params, true)
            .expect("wire conversion should succeed");

        assert_eq!(body["model"], "claude-upstream");
        assert_eq!(body["max_tokens"], 37);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"].as_array().unwrap().len(), 2);
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert!(body.get("future_top_level").is_none());
    }

    fn canonicalize_anthropic_content(body: &mut serde_json::Value) {
        fn string_to_text_blocks(value: &mut serde_json::Value) {
            let serde_json::Value::String(text) = value else {
                return;
            };
            *value = serde_json::json!([{"type": "text", "text": text}]);
        }

        if let Some(system) = body.get_mut("system") {
            string_to_text_blocks(system);
        }
        if let Some(messages) = body
            .get_mut("messages")
            .and_then(serde_json::Value::as_array_mut)
        {
            for message in messages {
                if let Some(content) = message.get_mut("content") {
                    string_to_text_blocks(content);
                }
            }
        }
    }

    const COMPAT_REQUEST_FIXTURES: &[(&str, &str)] = &[
        (
            "basic-system",
            include_str!("../../../../tests/fixtures/anthropic_compat/basic_system.json"),
        ),
        (
            "sampling-and-tools",
            include_str!("../../../../tests/fixtures/anthropic_compat/sampling_tools.json"),
        ),
        (
            "cached-tool-loop",
            include_str!("../../../../tests/fixtures/anthropic_compat/cached_tool_loop.json"),
        ),
        (
            "vision",
            include_str!("../../../../tests/fixtures/anthropic_compat/vision.json"),
        ),
    ];

    #[test]
    fn compat_request_matches_legacy_semantics_over_fixture_corpus() {
        let backend = AnthropicBackend::new();

        for (name, fixture) in COMPAT_REQUEST_FIXTURES {
            let wire: serde_json::Value = serde_json::from_str(fixture)
                .unwrap_or_else(|error| panic!("fixture {name} is invalid JSON: {error}"));
            let mut params: ChatCompletionParams = serde_json::from_value(wire.clone())
                .unwrap_or_else(|error| panic!("fixture {name} is not a chat request: {error}"));
            let stream = params.stream.unwrap_or(false);

            let mut legacy =
                serde_json::to_value(backend.build_request("claude-sonnet-4-6", &params, stream))
                    .unwrap_or_else(|error| {
                        panic!("legacy fixture {name} did not serialize: {error}")
                    });
            params.original_request = Some(wire);
            let mut compat = backend
                .build_request_body("claude-sonnet-4-6", &params, stream)
                .unwrap_or_else(|error| panic!("compat fixture {name} did not convert: {error:?}"));

            // The compatibility crate intentionally emits canonical block arrays
            // even when the legacy converter used a bare string. Normalize only
            // that representation detail; all protocol semantics must match.
            canonicalize_anthropic_content(&mut legacy);
            canonicalize_anthropic_content(&mut compat);
            assert_eq!(compat, legacy, "fixture {name} drifted");
        }
    }

    #[tokio::test]
    async fn chat_completion_uses_compat_body_and_shared_raw_transport() {
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(header("x-api-key", "upstream-secret"))
            .and(body_json(serde_json::json!({
                "model": "claude-upstream",
                "messages": [{
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}]
                }],
                "max_tokens": 37,
                "stream": false,
                "thinking": {"type": "enabled", "budget_tokens": 1024}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_compat_1",
                "type": "message",
                "role": "assistant",
                "model": "claude-upstream-dated",
                "content": [{"type": "text", "text": "done"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 2,
                    "cache_read_input_tokens": 3,
                    "cache_creation_input_tokens": 4
                }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new();
        let mut params = make_params(None, None);
        params.original_request = Some(serde_json::json!({
            "model": "anthropic/alias",
            "messages": [{"role": "user", "content": "hello"}],
            "max_completion_tokens": 37,
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "future_top_level": "not forwarded"
        }));
        let response = backend
            .chat_completion(&raw_test_config(server.uri()), "claude-upstream", params)
            .await
            .expect("converted completion should succeed");

        assert_eq!(response.response.id, "msg_compat_1");
        assert_eq!(response.response.usage.prompt_tokens, 17);
        assert_eq!(response.response.usage.cached_tokens(), 3);
        assert_eq!(response.response.usage.cache_creation_tokens(), 4);
    }

    #[tokio::test]
    async fn streaming_chat_completion_uses_shared_raw_transport() {
        use futures_util::StreamExt;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stream_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":4}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":4}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(body_json(serde_json::json!({
                "model": "claude-upstream",
                "messages": [{
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}]
                }],
                "max_tokens": 37,
                "stream": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new();
        let mut params = make_params(None, None);
        params.original_request = Some(serde_json::json!({
            "model": "anthropic/alias",
            "messages": [{"role": "user", "content": "hello"}],
            "max_completion_tokens": 37,
            "stream": true
        }));
        let mut stream = backend
            .chat_completion_stream(&raw_test_config(server.uri()), "claude-upstream", params)
            .await
            .expect("converted stream should start");

        let mut final_usage = None;
        while let Some(event) = stream.next().await {
            let event = event.expect("stream event should parse");
            if let Some(crate::StreamChunk::Chat(chunk)) = event.chunk {
                if chunk.usage.is_some() {
                    final_usage = chunk.usage;
                }
            }
        }
        let usage = final_usage.expect("stream should carry usage");
        assert_eq!(usage.prompt_tokens, 17);
        assert_eq!(usage.cached_tokens(), 3);
        assert_eq!(usage.cache_creation_tokens(), 4);
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
