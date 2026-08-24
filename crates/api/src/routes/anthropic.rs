//! Native Anthropic Messages routes.
//!
//! The request body stays schema-free on purpose. Cloud API validates only the
//! fields needed for routing, billing, and explicit feature gates, then the raw
//! transport rewrites `model` and forwards the rest to Anthropic.

use crate::middleware::auth::AuthenticatedApiKey;
use crate::models::AnthropicErrorResponse;
use crate::routes::api::AppState;
use crate::routes::common::{
    no_aliasing_requested, HEADER_MODEL_ALIAS_RESOLVED, HEADER_NO_ALIASING,
};
use axum::body::{Body, Bytes};
use axum::extract::{Extension, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use inference_providers::{
    anthropic_raw::AnthropicRawBody, AnthropicRawEndpoint, AnthropicRawError, AnthropicRawHeaders,
    AnthropicRawRequest, AnthropicRawResponse,
};
use services::completions::ports::{CompletionError, ConcurrentRequestGuard};
use services::models::{ModelWithPricing, ModelsError, ModelsServiceTrait};
use services::usage::{
    five_minute_cache_write_rate, CacheWriteBilling, InferenceType, ProviderAttribution,
    RecordUsageServiceRequest, StopReason, UsageServiceTrait,
};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use uuid::Uuid;

const MAX_SSE_USAGE_LINE_BYTES: usize = 256 * 1024;
const ALLOWED_ANTHROPIC_BETAS: &[&str] = &[
    // Current Claude Code transport marker and token-only request controls.
    "claude-code-20250219",
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "effort-2025-11-24",
    "structured-outputs-2025-12-15",
    "fine-grained-tool-streaming-2025-05-14",
    "token-efficient-tools-2025-02-19",
    "prompt-caching-2024-07-31",
    // Claude Code currently sends this marker on ordinary requests. The
    // separate typed-tool body gate still rejects invocation of the advisor.
    "advisor-tool-2026-03-01",
];

struct PreparedRequest {
    body: serde_json::Value,
    requested_model: String,
    stream: bool,
    beta_query: bool,
    headers: AnthropicRawHeaders,
}

#[derive(Debug)]
struct AnthropicRouteError {
    status: StatusCode,
    error_type: String,
    message: String,
}

impl AnthropicRouteError {
    fn new(status: StatusCode, error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            error_type: error_type.into(),
            message: message.into(),
        }
    }
}

impl IntoResponse for AnthropicRouteError {
    fn into_response(self) -> Response {
        anthropic_error(self.status, self.error_type, self.message)
    }
}

type RouteResult<T> = Result<T, AnthropicRouteError>;

#[derive(Debug, Default, Clone)]
struct NativeUsage {
    provider_request_id: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_input_tokens: i64,
    cache_creation_input_tokens: i64,
    stop_reason: Option<StopReason>,
}

impl NativeUsage {
    fn apply_usage(&mut self, usage: &serde_json::Value) {
        if let Some(usage) = usage.as_object() {
            self.apply_usage_object(usage);
        }
    }

    fn apply_usage_object(&mut self, usage: &serde_json::Map<String, serde_json::Value>) {
        update_non_negative(&mut self.input_tokens, usage.get("input_tokens"));
        update_non_negative(&mut self.output_tokens, usage.get("output_tokens"));
        update_non_negative(
            &mut self.cache_read_input_tokens,
            usage.get("cache_read_input_tokens"),
        );
        update_non_negative(
            &mut self.cache_creation_input_tokens,
            usage.get("cache_creation_input_tokens"),
        );
    }

    fn input_tokens_for_billing(&self) -> i32 {
        saturating_token_count(
            self.input_tokens
                .saturating_add(self.cache_read_input_tokens)
                .saturating_add(self.cache_creation_input_tokens),
        )
    }

    fn output_tokens_for_billing(&self) -> i32 {
        saturating_token_count(self.output_tokens)
    }

    fn cache_read_tokens_for_billing(&self) -> i32 {
        saturating_token_count(self.cache_read_input_tokens).min(self.input_tokens_for_billing())
    }

    fn cache_write_tokens_for_billing(&self) -> i32 {
        let input_tokens = self.input_tokens_for_billing();
        saturating_token_count(self.cache_creation_input_tokens)
            .min(input_tokens.saturating_sub(self.cache_read_tokens_for_billing()))
    }
}

#[derive(Clone)]
struct NativeBillingContext {
    usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    inference_type: InferenceType,
    provider_attribution: ProviderAttribution,
    cache_write_cost_per_token: i64,
}

#[derive(Debug, PartialEq)]
struct NativeBillingSeed {
    api_key_id: Uuid,
    cache_write_cost_per_token: i64,
}

#[derive(Default)]
struct SseUsageParser {
    line_buffer: Vec<u8>,
    usage: NativeUsage,
    discarding_oversized_line: bool,
}

impl SseUsageParser {
    fn push(&mut self, bytes: &[u8]) {
        let mut bytes = bytes;
        if self.discarding_oversized_line {
            let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
                return;
            };
            self.discarding_oversized_line = false;
            bytes = &bytes[newline + 1..];
        }
        self.line_buffer.extend_from_slice(bytes);
        if self.line_buffer.len() > MAX_SSE_USAGE_LINE_BYTES && !self.line_buffer.contains(&b'\n') {
            self.discarding_oversized_line = true;
            self.line_buffer.clear();
            tracing::warn!("Native Anthropic usage tee skipped an oversized SSE line");
            return;
        }

        while let Some(newline) = self.line_buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.line_buffer.drain(..=newline).collect::<Vec<_>>();
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            self.process_line(&line);
        }
    }

    fn finish(&mut self) -> NativeUsage {
        if !self.discarding_oversized_line && !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.process_line(&line);
        }
        std::mem::take(&mut self.usage)
    }

    fn process_line(&mut self, line: &[u8]) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if data == b"[DONE]" {
            return;
        }
        let Ok(event) = serde_json::from_slice::<serde_json::Value>(data) else {
            return;
        };
        self.apply_event(&event);
    }

    fn apply_event(&mut self, event: &serde_json::Value) {
        match event.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => {
                let Some(message) = event.get("message") else {
                    return;
                };
                self.usage.provider_request_id = message
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                if let Some(usage) = message.get("usage") {
                    self.usage.apply_usage(usage);
                }
            }
            Some("message_delta") => {
                if let Some(usage) = event.get("usage") {
                    self.usage.apply_usage(usage);
                }
                if let Some(reason) = event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(serde_json::Value::as_str)
                {
                    self.usage.stop_reason = Some(map_stop_reason(reason));
                }
            }
            Some("error") => self.usage.stop_reason = Some(StopReason::ProviderError),
            _ => {}
        }
    }
}

struct NativeUsageStream {
    inner: AnthropicRawBody,
    parser: SseUsageParser,
    billing: Option<NativeBillingContext>,
    concurrent_slot: Option<ConcurrentRequestGuard>,
    runtime_handle: tokio::runtime::Handle,
}

impl NativeUsageStream {
    fn finish_billing(&mut self, default_reason: StopReason) {
        // The upstream request no longer occupies provider capacity after its
        // stream ends, errors, or is dropped by a disconnected client.
        self.concurrent_slot.take();
        let Some(context) = self.billing.take() else {
            return;
        };
        let mut usage = self.parser.finish();
        let stop_reason = usage.stop_reason.take().unwrap_or(default_reason);
        spawn_native_usage_recording(self.runtime_handle.clone(), context, usage, stop_reason);
    }
}

fn spawn_native_usage_recording(
    handle: tokio::runtime::Handle,
    context: NativeBillingContext,
    usage: NativeUsage,
    stop_reason: StopReason,
) {
    let task_handle = handle.clone();
    let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle.spawn_blocking(move || {
            task_handle.block_on(async move {
                match tokio::time::timeout(
                    Duration::from_secs(2),
                    record_native_usage(context, usage, stop_reason),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::error!(error = %error, "Failed to record native Anthropic stream usage");
                    }
                    Err(_) => {
                        tracing::error!("Timed out recording native Anthropic stream usage");
                    }
                }
            });
        });
    }));
    if spawned.is_err() {
        tracing::error!("Could not schedule native Anthropic usage recording during shutdown");
    }
}

impl Stream for NativeUsageStream {
    type Item = Result<Bytes, AnthropicRawError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                this.parser.push(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.finish_billing(StopReason::ProviderError);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finish_billing(StopReason::Completed);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for NativeUsageStream {
    fn drop(&mut self) {
        self.finish_billing(StopReason::ClientDisconnect);
    }
}

pub async fn messages(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_request(
        app_state,
        api_key,
        query,
        headers,
        body,
        AnthropicRawEndpoint::Messages,
    )
    .await
}

pub async fn count_tokens(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_request(
        app_state,
        api_key,
        query,
        headers,
        body,
        AnthropicRawEndpoint::CountTokens,
    )
    .await
}

async fn handle_request(
    app_state: AppState,
    api_key: AuthenticatedApiKey,
    query: Option<String>,
    headers: HeaderMap,
    body: Bytes,
    endpoint: AnthropicRawEndpoint,
) -> Response {
    let prepared = match prepare_request(
        &headers,
        query.as_deref(),
        &body,
        endpoint,
        &app_state.config.external_providers.anthropic_allowed_betas,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return error.into_response(),
    };

    let (model, alias_from) = match resolve_model(
        app_state.models_service.as_ref(),
        &headers,
        &prepared.requested_model,
    )
    .await
    {
        Ok(model) => model,
        Err(response) => return response,
    };

    let billing_seed = match prepare_billing_seed(
        endpoint,
        &api_key.api_key.id.0,
        model.input_cost_per_token,
    ) {
        Ok(seed) => seed,
        Err(error) => {
            tracing::error!(model = %model.model_name, "Invalid native Anthropic billing context");
            return error.into_response();
        }
    };

    let concurrent_slot = if endpoint == AnthropicRawEndpoint::Messages {
        match app_state
            .completion_service
            .acquire_concurrent_slot(api_key.organization.id.0, model.id, &model.model_name)
            .await
        {
            Ok(slot) => Some(slot),
            Err(CompletionError::RateLimitExceeded(message)) => {
                return anthropic_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", message);
            }
            Err(error) => {
                tracing::error!(error = %error, model = %model.model_name, "Failed to acquire native Anthropic concurrency slot");
                return anthropic_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "Failed to enforce the organization concurrency limit",
                );
            }
        }
    } else {
        None
    };

    let request = AnthropicRawRequest {
        endpoint,
        beta: prepared.beta_query,
        body: prepared.body,
        headers: prepared.headers,
    };
    let served = match app_state
        .inference_provider_pool
        .anthropic_raw(&model.model_name, request)
        .await
    {
        Ok(response) => response,
        Err(AnthropicRawError::UnsupportedProvider) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "The requested model is not available through the Anthropic Messages API",
            );
        }
        Err(AnthropicRawError::InvalidRequest(message)) => {
            return anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", message);
        }
        Err(AnthropicRawError::Transport(error)) => {
            tracing::error!(error = %error, model = %model.model_name, "Native Anthropic transport failed");
            return anthropic_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "Anthropic upstream request failed",
            );
        }
    };

    let billing = billing_seed.map(|seed| NativeBillingContext {
        usage_service: app_state.usage_service,
        organization_id: api_key.organization.id.0,
        workspace_id: api_key.workspace.id.0,
        api_key_id: seed.api_key_id,
        model_id: model.id,
        inference_type: if prepared.stream {
            InferenceType::ChatCompletionStream
        } else {
            InferenceType::ChatCompletion
        },
        provider_attribution: served.provider_attribution,
        cache_write_cost_per_token: seed.cache_write_cost_per_token,
    });

    build_upstream_response(
        served.response,
        endpoint,
        prepared.stream,
        billing,
        concurrent_slot,
        alias_from,
    )
    .await
}

fn prepare_billing_seed(
    endpoint: AnthropicRawEndpoint,
    api_key_id: &str,
    input_cost_per_token: i64,
) -> RouteResult<Option<NativeBillingSeed>> {
    if endpoint == AnthropicRawEndpoint::CountTokens {
        return Ok(None);
    }

    let api_key_id = Uuid::parse_str(api_key_id).map_err(|_| {
        AnthropicRouteError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "Internal server error",
        )
    })?;
    let cache_write_cost_per_token = five_minute_cache_write_rate(input_cost_per_token)
        .ok_or_else(|| {
            AnthropicRouteError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "The requested model has invalid cache pricing",
            )
        })?;

    Ok(Some(NativeBillingSeed {
        api_key_id,
        cache_write_cost_per_token,
    }))
}

fn prepare_request(
    headers: &HeaderMap,
    query: Option<&str>,
    body: &[u8],
    endpoint: AnthropicRawEndpoint,
    additional_allowed_betas: &[String],
) -> RouteResult<PreparedRequest> {
    reject_unsupported_anthropic_headers(headers)?;
    reject_e2ee(headers)?;

    let body: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
        AnthropicRouteError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Request body must be valid JSON",
        )
    })?;
    let object = body.as_object().ok_or_else(|| {
        AnthropicRouteError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Request body must be a JSON object",
        )
    })?;

    reject_unsupported_features(headers, object)?;

    let requested_model = object
        .get("model")
        .and_then(serde_json::Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            AnthropicRouteError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model must be a non-empty string",
            )
        })?;

    let stream = if endpoint == AnthropicRawEndpoint::Messages {
        match object.get("stream") {
            Some(serde_json::Value::Bool(value)) => *value,
            Some(_) => {
                return Err(AnthropicRouteError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "stream must be a boolean",
                ));
            }
            None => false,
        }
    } else {
        false
    };

    Ok(PreparedRequest {
        body,
        requested_model,
        stream,
        beta_query: normalize_query(query)?,
        headers: AnthropicRawHeaders {
            version: single_header(headers, "anthropic-version")?,
            beta: normalized_beta_header(headers, additional_allowed_betas)?,
        },
    })
}

fn normalize_query(query: Option<&str>) -> RouteResult<bool> {
    match query.filter(|query| !query.is_empty()) {
        None => Ok(false),
        Some("beta=true") => Ok(true),
        Some(_) => Err(AnthropicRouteError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Only the beta=true query parameter is supported",
        )),
    }
}

fn reject_unsupported_features(
    headers: &HeaderMap,
    body: &serde_json::Map<String, serde_json::Value>,
) -> RouteResult<()> {
    if headers.contains_key(services::auto_redact::AUTO_REDACT_HEADER)
        || body.contains_key(services::auto_redact::AUTO_REDACT_BODY_FIELD)
    {
        return Err(unsupported_feature("auto_redact"));
    }
    if body.get("speed").and_then(serde_json::Value::as_str) == Some("fast") {
        return Err(unsupported_feature("speed=fast"));
    }
    if body
        .get("service_tier")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|tier| tier != "standard_only")
    {
        return Err(unsupported_feature(
            "service_tier values other than standard_only",
        ));
    }
    if body.contains_key("inference_geo") {
        return Err(unsupported_feature("inference_geo"));
    }
    if body.contains_key("mcp_servers") {
        return Err(unsupported_feature("mcp_servers"));
    }
    if body.contains_key("container") {
        return Err(unsupported_feature("container"));
    }
    if body.contains_key("fallbacks") {
        return Err(unsupported_feature("server-side fallbacks"));
    }
    if let Some(tools) = body.get("tools").and_then(serde_json::Value::as_array) {
        // Client-executed tools may omit their type or explicitly use
        // `type: "custom"`. Other typed tools are Anthropic-hosted products;
        // reject those so new billable server features cannot bypass policy.
        if tools.iter().any(|tool| {
            tool.get("type")
                .is_some_and(|kind| kind.as_str() != Some("custom"))
        }) {
            return Err(unsupported_feature("typed Anthropic tools"));
        }
    }
    if request_contains_one_hour_cache_control(body) {
        return Err(unsupported_feature("one-hour prompt caching"));
    }
    Ok(())
}

fn value_has_one_hour_cache_control(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            object.get("cache_control").is_some_and(|cache_control| {
                cache_control.get("ttl").and_then(serde_json::Value::as_str) == Some("1h")
            })
        }
        serde_json::Value::Array(values) => values.iter().any(value_has_one_hour_cache_control),
        _ => false,
    }
}

fn request_contains_one_hour_cache_control(
    body: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    body.get("system")
        .is_some_and(value_has_one_hour_cache_control)
        || body
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    message
                        .get("content")
                        .is_some_and(value_has_one_hour_cache_control)
                })
            })
        || body
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|tools| tools.iter().any(value_has_one_hour_cache_control))
}

fn unsupported_feature(feature: &str) -> AnthropicRouteError {
    AnthropicRouteError::new(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        format!("{feature} is not supported by the native Anthropic endpoint yet"),
    )
}

fn reject_e2ee(headers: &HeaderMap) -> RouteResult<()> {
    const E2EE_HEADERS: &[&str] = &[
        "x-signing-algo",
        "x-client-pub-key",
        "x-model-pub-key",
        "x-encryption-version",
        "x-encrypt-all-fields",
    ];
    if E2EE_HEADERS.iter().any(|name| headers.contains_key(*name)) {
        return Err(unsupported_feature("E2EE and attestation headers"));
    }
    Ok(())
}

fn reject_unsupported_anthropic_headers(headers: &HeaderMap) -> RouteResult<()> {
    for name in headers.keys() {
        let name = name.as_str();
        if name.starts_with("anthropic-")
            && !matches!(
                name,
                "anthropic-beta"
                    | "anthropic-dangerous-direct-browser-access"
                    | "anthropic-version"
            )
        {
            return Err(unsupported_feature(name));
        }
    }
    Ok(())
}

fn single_header(headers: &HeaderMap, name: &'static str) -> RouteResult<Option<String>> {
    let values = headers.get_all(name);
    let mut iter = values.iter();
    let Some(value) = iter.next() else {
        return Ok(None);
    };
    if iter.next().is_some() {
        return Err(AnthropicRouteError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("{name} must not be repeated"),
        ));
    }
    value
        .to_str()
        .map(|value| Some(value.to_string()))
        .map_err(|_| {
            AnthropicRouteError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("{name} must contain visible ASCII characters"),
            )
        })
}

fn normalized_beta_header(
    headers: &HeaderMap,
    additional_allowed_betas: &[String],
) -> RouteResult<Option<String>> {
    // Keep the default surface narrow, while allowing operators to admit a
    // newly released token through ANTHROPIC_ALLOWED_BETAS without a code
    // deployment. Body policy still blocks unsupported server-side products.
    let mut betas = Vec::<String>::new();
    for value in headers.get_all("anthropic-beta") {
        let value = value.to_str().map_err(|_| {
            AnthropicRouteError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "anthropic-beta must contain visible ASCII characters",
            )
        })?;
        for token in value
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            if !ALLOWED_ANTHROPIC_BETAS.contains(&token)
                && !additional_allowed_betas
                    .iter()
                    .any(|allowed| allowed == token)
            {
                return Err(unsupported_feature(&format!(
                    "anthropic-beta token '{token}'"
                )));
            }
            if !betas.iter().any(|existing| existing == token) {
                betas.push(token.to_string());
            }
        }
    }
    Ok((!betas.is_empty()).then(|| betas.join(",")))
}

async fn resolve_model(
    models_service: &dyn ModelsServiceTrait,
    headers: &HeaderMap,
    requested: &str,
) -> Result<(ModelWithPricing, Option<String>), Response> {
    let normalized = if requested.starts_with("anthropic/") {
        requested.to_string()
    } else {
        format!("anthropic/{requested}")
    };

    let model = match models_service.resolve_and_get_model(&normalized).await {
        Ok(model) => model,
        Err(ModelsError::NotFound(_)) if normalized != requested => models_service
            .resolve_and_get_model(requested)
            .await
            .map_err(model_resolution_error)?,
        Err(error) => return Err(model_resolution_error(error)),
    };

    let anthropic_backend = model.provider_type == "external"
        && model.model_name.starts_with("anthropic/")
        && model
            .provider_config
            .as_ref()
            .and_then(|config| config.get("backend"))
            .and_then(serde_json::Value::as_str)
            == Some("anthropic");
    if !anthropic_backend {
        return Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "The native Messages endpoint accepts Anthropic-backed models only",
        ));
    }

    let resolved_from_alias = model.model_name != normalized;
    if resolved_from_alias && no_aliasing_requested(headers) {
        return Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!(
                "Model '{requested}' resolves to '{}' and the request set {HEADER_NO_ALIASING}",
                model.model_name
            ),
        ));
    }
    let alias_header = resolved_from_alias.then(|| format!("{normalized} -> {}", model.model_name));
    Ok((model, alias_header))
}

fn model_resolution_error(error: ModelsError) -> Response {
    match error {
        ModelsError::NotFound(_) => anthropic_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "The requested model was not found",
        ),
        other => {
            tracing::error!(error = %other, "Failed to resolve native Anthropic model");
            anthropic_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "Failed to resolve model",
            )
        }
    }
}

async fn build_upstream_response(
    mut upstream: AnthropicRawResponse,
    endpoint: AnthropicRawEndpoint,
    stream: bool,
    billing: Option<NativeBillingContext>,
    concurrent_slot: Option<ConcurrentRequestGuard>,
    alias_from: Option<String>,
) -> Response {
    let status = upstream.status;
    if status.is_success() && endpoint == AnthropicRawEndpoint::Messages && !stream {
        let bytes = match collect_body(&mut upstream).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(error = %error, "Failed to read native Anthropic response body");
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Anthropic upstream response was interrupted",
                );
            }
        };
        let usage = match parse_non_stream_usage(&bytes) {
            Ok(usage) => usage,
            Err(error) => {
                tracing::error!(error, "Native Anthropic response omitted valid usage");
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Anthropic upstream response omitted valid usage",
                );
            }
        };
        let reason = usage.stop_reason.clone().unwrap_or(StopReason::Completed);
        if let Some(billing) = billing {
            if let Err(error) = record_native_usage(billing, usage, reason).await {
                tracing::error!(error = %error, "Failed to record native Anthropic usage");
                return anthropic_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "Failed to record usage",
                );
            }
        }
        return response_from_bytes(status, upstream.headers, bytes, alias_from);
    }

    let body = if status.is_success() && endpoint == AnthropicRawEndpoint::Messages && stream {
        Body::from_stream(NativeUsageStream {
            inner: upstream.body,
            parser: SseUsageParser::default(),
            billing,
            concurrent_slot,
            runtime_handle: tokio::runtime::Handle::current(),
        })
    } else {
        Body::from_stream(upstream.body)
    };
    response_from_body(status, upstream.headers, body, alias_from)
}

async fn collect_body(upstream: &mut AnthropicRawResponse) -> Result<Vec<u8>, AnthropicRawError> {
    use futures_util::StreamExt as _;
    let mut body = Vec::new();
    while let Some(chunk) = upstream.body.next().await {
        body.extend_from_slice(&chunk?);
    }
    Ok(body)
}

fn parse_non_stream_usage(body: &[u8]) -> Result<NativeUsage, &'static str> {
    let response = serde_json::from_slice::<serde_json::Value>(body)
        .map_err(|_| "response body was not valid JSON")?;
    let raw_usage = response
        .get("usage")
        .and_then(serde_json::Value::as_object)
        .ok_or("response body did not contain a usage object")?;
    let input_tokens = required_non_negative_token_count(raw_usage, "input_tokens")?;
    let output_tokens = required_non_negative_token_count(raw_usage, "output_tokens")?;
    let mut usage = NativeUsage {
        provider_request_id: response
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        input_tokens,
        output_tokens,
        stop_reason: response
            .get("stop_reason")
            .and_then(serde_json::Value::as_str)
            .map(map_stop_reason),
        ..Default::default()
    };
    usage.apply_usage_object(raw_usage);
    Ok(usage)
}

fn required_non_negative_token_count(
    usage: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<i64, &'static str> {
    usage
        .get(field)
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or("usage token counts must be non-negative integers")
}

async fn record_native_usage(
    context: NativeBillingContext,
    usage: NativeUsage,
    stop_reason: StopReason,
) -> Result<(), services::usage::UsageError> {
    let input_tokens = usage.input_tokens_for_billing();
    let output_tokens = usage.output_tokens_for_billing();
    if input_tokens == 0 && output_tokens == 0 {
        tracing::warn!("Native Anthropic response carried no billable usage");
        return Ok(());
    }

    let request = RecordUsageServiceRequest {
        organization_id: context.organization_id,
        workspace_id: context.workspace_id,
        api_key_id: context.api_key_id,
        model_id: context.model_id,
        input_tokens,
        output_tokens,
        cache_read_tokens: usage.cache_read_tokens_for_billing(),
        cache_write: (usage.cache_write_tokens_for_billing() > 0).then_some(CacheWriteBilling {
            tokens: usage.cache_write_tokens_for_billing(),
            cost_per_token: context.cache_write_cost_per_token,
        }),
        profiled_cache_write_tokens: 0,
        requested_service_tier: None,
        provider_service_tier: None,
        inference_type: context.inference_type,
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(Uuid::new_v4()),
        provider_request_id: usage.provider_request_id,
        stop_reason: Some(stop_reason),
        response_id: None,
        image_count: None,
        provider_attribution: context.provider_attribution,
    };

    context
        .usage_service
        .record_usage(request)
        .await
        .map(|_| ())
}

fn response_from_bytes(
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
    alias_from: Option<String>,
) -> Response {
    response_from_body(status, headers, Body::from(body), alias_from)
}

fn response_from_body(
    status: StatusCode,
    headers: HeaderMap,
    body: Body,
    alias_from: Option<String>,
) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    for (name, value) in headers {
        let Some(name) = name else { continue };
        if is_forwardable_upstream_header(&name) {
            response.headers_mut().append(name, value);
        }
    }
    response.headers_mut().insert(
        "x-serving-provider",
        HeaderValue::from_static("non-attested"),
    );
    if let Some(alias) = alias_from.and_then(|alias| HeaderValue::from_str(&alias).ok()) {
        response
            .headers_mut()
            .insert(HEADER_MODEL_ALIAS_RESOLVED, alias);
    }
    response
}

fn is_forwardable_upstream_header(name: &header::HeaderName) -> bool {
    matches!(name.as_str(), "content-type" | "request-id" | "retry-after")
}

fn anthropic_error(
    status: StatusCode,
    error_type: impl Into<String>,
    message: impl Into<String>,
) -> Response {
    (
        status,
        axum::Json(AnthropicErrorResponse::new(error_type, message)),
    )
        .into_response()
}

fn update_non_negative(target: &mut i64, value: Option<&serde_json::Value>) {
    if let Some(value) = value
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value >= 0)
    {
        *target = value;
    }
}

fn saturating_token_count(value: i64) -> i32 {
    i32::try_from(value.max(0)).unwrap_or(i32::MAX)
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::Completed,
        "max_tokens" => StopReason::Length,
        "refusal" => StopReason::ContentFilter,
        "stop_sequence" => StopReason::Stop,
        "tool_use" => StopReason::ToolCalls,
        other => StopReason::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers
    }

    fn base_body() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap()
    }

    #[test]
    fn preparation_keeps_unknown_body_fields() {
        let mut body: serde_json::Value = serde_json::from_slice(&base_body()).unwrap();
        body["future_field"] = serde_json::json!({"nested": [1, 2, 3]});
        let prepared = prepare_request(
            &request_headers(),
            None,
            &serde_json::to_vec(&body).unwrap(),
            AnthropicRawEndpoint::Messages,
            &[],
        )
        .unwrap();

        assert_eq!(prepared.body["future_field"], body["future_field"]);
        assert_eq!(prepared.requested_model, "claude-sonnet-4-6");
        assert!(!prepared.stream);
    }

    #[test]
    fn beta_tokens_are_preserved_and_deduplicated() {
        let mut headers = request_headers();
        headers.append(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219,interleaved-thinking-2025-05-14"),
        );
        headers.append(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219"),
        );
        assert_eq!(
            normalized_beta_header(&headers, &[]).unwrap().as_deref(),
            Some("claude-code-20250219,interleaved-thinking-2025-05-14")
        );

        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("fast-mode-2026-02-01"),
        );
        assert!(normalized_beta_header(&headers, &[]).is_err());

        let additional = vec!["fast-mode-2026-02-01".to_string()];
        assert_eq!(
            normalized_beta_header(&headers, &additional)
                .unwrap()
                .as_deref(),
            Some("fast-mode-2026-02-01")
        );
    }

    #[test]
    fn current_claude_code_beta_set_and_query_are_accepted() {
        let mut headers = request_headers();
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static(
                "claude-code-20250219,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,effort-2025-11-24,structured-outputs-2025-12-15",
            ),
        );
        let prepared = prepare_request(
            &headers,
            Some("beta=true"),
            &base_body(),
            AnthropicRawEndpoint::Messages,
            &[],
        )
        .unwrap();

        assert!(prepared.beta_query);
        assert_eq!(
            prepared.headers.beta.as_deref(),
            headers
                .get("anthropic-beta")
                .and_then(|value| value.to_str().ok())
        );
        assert!(normalize_query(Some("future=true")).is_err());
    }

    #[test]
    fn premium_and_server_features_are_rejected_but_client_tools_are_allowed() {
        let headers = request_headers();
        let client_tool = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "tools": [
                {"name": "lookup", "input_schema": {"type": "object"}},
                {"type": "custom", "name": "explicit", "input_schema": {"type": "object"}}
            ]
        });
        assert!(reject_unsupported_features(&headers, client_tool.as_object().unwrap()).is_ok());

        for body in [
            serde_json::json!({"speed": "fast"}),
            serde_json::json!({"service_tier": "auto"}),
            serde_json::json!({"inference_geo": "us"}),
            serde_json::json!({"mcp_servers": []}),
            serde_json::json!({"tools": [{"type": "web_search_20260209", "name": "web_search"}]}),
            serde_json::json!({"tools": [{"type": "future_server_tool", "name": "future"}]}),
            serde_json::json!({"container": "container_1"}),
            serde_json::json!({"fallbacks": [{"model": "claude-fallback"}]}),
            serde_json::json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}]}),
        ] {
            assert!(reject_unsupported_features(&headers, body.as_object().unwrap()).is_err());
        }

        let tool_input_with_cache_shaped_data = serde_json::json!({
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "name": "remember",
                    "input": {"cache_control": {"ttl": "1h"}}
                }]
            }]
        });
        assert!(reject_unsupported_features(
            &headers,
            tool_input_with_cache_shaped_data.as_object().unwrap(),
        )
        .is_ok());
    }

    #[test]
    fn non_stream_usage_keeps_anthropic_cache_accounting_invariants() {
        let usage = parse_non_stream_usage(
            br#"{"id":"msg_1","stop_reason":"end_turn","usage":{"input_tokens":10,"cache_read_input_tokens":80,"cache_creation_input_tokens":5,"output_tokens":7}}"#,
        )
        .unwrap();
        assert_eq!(usage.input_tokens_for_billing(), 95);
        assert_eq!(usage.cache_read_tokens_for_billing(), 80);
        assert_eq!(usage.cache_write_tokens_for_billing(), 5);
        assert_eq!(usage.output_tokens_for_billing(), 7);
        assert_eq!(usage.provider_request_id.as_deref(), Some("msg_1"));
        assert_eq!(usage.stop_reason, Some(StopReason::Completed));
    }

    #[test]
    fn non_stream_usage_rejects_missing_or_invalid_required_counts() {
        for body in [
            br#"{}"#.as_slice(),
            br#"{"usage":{}}"#.as_slice(),
            br#"{"usage":{"input_tokens":1,"output_tokens":-1}}"#.as_slice(),
            br#"{"usage":{"input_tokens":"1","output_tokens":1}}"#.as_slice(),
            br#"not-json"#.as_slice(),
        ] {
            assert!(parse_non_stream_usage(body).is_err());
        }
    }

    #[test]
    fn five_minute_cache_write_rate_rounds_non_divisible_prices() {
        assert_eq!(five_minute_cache_write_rate(3_000), Some(3_750));
        assert_eq!(five_minute_cache_write_rate(1), Some(1));
        assert_eq!(five_minute_cache_write_rate(-1), None);
        assert_eq!(five_minute_cache_write_rate(i64::MAX), None);
    }

    #[test]
    fn count_tokens_never_requires_or_creates_billing_state() {
        assert_eq!(
            prepare_billing_seed(AnthropicRawEndpoint::CountTokens, "not-a-uuid", -1).unwrap(),
            None
        );

        let api_key_id = Uuid::new_v4();
        let seed = prepare_billing_seed(
            AnthropicRawEndpoint::Messages,
            &api_key_id.to_string(),
            3_000,
        )
        .unwrap()
        .unwrap();
        assert_eq!(seed.api_key_id, api_key_id);
        assert_eq!(seed.cache_write_cost_per_token, 3_750);
    }

    #[test]
    fn sse_usage_parser_handles_fragmented_events() {
        let mut parser = SseUsageParser::default();
        parser.push(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stream\",\"usage\":{\"input_tokens\":4,\"cache_read_input_tokens\":20,\"cache_creation_input_tokens\":3}}}\n\nevent: message_delta\nda");
        parser.push(b"ta: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n");
        let usage = parser.finish();

        assert_eq!(usage.provider_request_id.as_deref(), Some("msg_stream"));
        assert_eq!(usage.input_tokens_for_billing(), 27);
        assert_eq!(usage.cache_read_tokens_for_billing(), 20);
        assert_eq!(usage.cache_write_tokens_for_billing(), 3);
        assert_eq!(usage.output_tokens_for_billing(), 9);
        assert_eq!(usage.stop_reason, Some(StopReason::ToolCalls));
    }

    #[test]
    fn sse_usage_parser_resumes_after_oversized_content_line() {
        let mut parser = SseUsageParser::default();
        parser.push(
            b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stream\",\"usage\":{\"input_tokens\":4}}}\n",
        );
        parser.push(&vec![b'x'; MAX_SSE_USAGE_LINE_BYTES + 1]);
        parser.push(
            b"\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n",
        );
        let usage = parser.finish();

        assert_eq!(usage.input_tokens_for_billing(), 4);
        assert_eq!(usage.output_tokens_for_billing(), 9);
        assert_eq!(usage.stop_reason, Some(StopReason::Completed));
    }

    #[test]
    fn upstream_response_headers_use_a_small_safe_allowlist() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("123"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("7"));
        headers.insert("request-id", HeaderValue::from_static("req_synthetic"));
        headers.insert(
            "anthropic-organization-id",
            HeaderValue::from_static("org_synthetic"),
        );
        headers.insert(
            "anthropic-ratelimit-requests-remaining",
            HeaderValue::from_static("42"),
        );
        let response = response_from_bytes(
            StatusCode::TOO_MANY_REQUESTS,
            headers,
            br#"{"type":"error"}"#.to_vec(),
            None,
        );
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "7");
        assert_eq!(
            response.headers().get("request-id").unwrap(),
            "req_synthetic"
        );
        assert!(response
            .headers()
            .get("anthropic-organization-id")
            .is_none());
        assert!(response
            .headers()
            .get("anthropic-ratelimit-requests-remaining")
            .is_none());
    }
}
