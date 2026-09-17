//! Narrow native Responses path for explicitly stateless Astra requests.
//! Stateful requests and Chat Completions never enter this module.
use super::{api::AppState, common, completions::HEADER_INFERENCE_ID};
use crate::{middleware::auth::AuthenticatedApiKey, models::ErrorResponse};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use inference_providers::{
    responses_raw::{ResponsesRawBody, ResponsesRawResponse},
    CompletionError,
};
use serde_json::{json, Value};
use services::{
    completions::{hash_inference_id_to_uuid, ports::ConcurrentRequestGuard},
    usage::{
        InferenceType, ProviderAttribution, RecordUsageServiceRequest, StopReason, TextServiceTier,
    },
};
use sha2::{Digest, Sha256};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use uuid::Uuid;

pub(super) fn selected(body: &Value) -> bool {
    body["model"]
        .as_str()
        .and_then(|m| m.strip_prefix("openai/"))
        .is_some_and(inference_providers::responses_raw::is_astra)
        && inference_providers::responses_raw::is_stateless(body)
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(ErrorResponse::new(
            message.into(),
            if status.is_client_error() {
                "invalid_request_error"
            } else {
                "api_error"
            }
            .into(),
        )),
    )
        .into_response()
}

/// Preserve native fields; only validate gateway-supported features and policy.
fn validate(body: &Value) -> Result<(), &'static str> {
    for key in ["stream", "background"] {
        if body
            .get(key)
            .is_some_and(|v| !v.is_null() && !v.is_boolean())
        {
            return Err("stream and background must be booleans");
        }
    }
    if body
        .get("service_tier")
        .is_some_and(|v| !v.is_null() && v != "auto" && v != "default")
    {
        return Err("service_tier must be 'auto' or 'default' for /v1/responses");
    }
    if body
        .get("max_output_tokens")
        .is_some_and(|v| !v.is_null() && v.as_i64().is_none_or(|n| n <= 0))
    {
        return Err("max_output_tokens must be a positive integer");
    }
    if body
        .get("instructions")
        .is_some_and(|v| !v.is_null() && !v.is_string())
    {
        return Err("instructions must be a string");
    }
    if let Some(tools) = body.get("tools").filter(|v| !v.is_null()) {
        let tools = tools.as_array().ok_or("tools must be an array")?;
        if tools.iter().any(|t| t["type"] != "function") {
            return Err(
                "Native stateless Astra Responses supports client-managed function tools only",
            );
        }
    }
    if body.get("prompt").is_some_and(|v| !v.is_null()) {
        return Err("Stored prompt references are unavailable for native stateless Responses");
    }
    // Cloud file IDs and built-in/MCP continuation items are not OpenAI IDs.
    if let Some(input) = body.get("input") {
        if !input.is_string() && !input.is_array() {
            return Err("input must be a string or array");
        }
        for item in input.as_array().into_iter().flatten() {
            if !matches!(
                item["type"].as_str(),
                None | Some("message" | "function_call" | "function_call_output" | "reasoning")
            ) {
                return Err("Unsupported input item for native stateless Astra Responses");
            }
            if ["content", "output"]
                .into_iter()
                .filter_map(|field| item[field].as_array())
                .flatten()
                .any(|p| p["type"] == "input_file" || p.get("file_id").is_some())
            {
                return Err(
                    "File references are not supported by native stateless Astra Responses",
                );
            }
        }
    }
    Ok(())
}

pub(super) async fn handle(
    app: AppState,
    api_key: AuthenticatedApiKey,
    headers: HeaderMap,
    request_hash: String,
    mut body: Value,
) -> Response {
    if let Err(message) = validate(&body) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let encryption = match common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(response) => return response.into_response(),
    };
    if encryption.signing_algo.is_some()
        || encryption.client_pub_key.is_some()
        || encryption.model_pub_key.is_some()
        || encryption.encryption_version.is_some()
        || encryption.encrypt_all_fields.is_some()
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Encryption headers are unavailable for native OpenAI Responses",
        );
    }
    let requested = body["model"]
        .as_str()
        .expect("selected request has a model");
    let model = match app.models_service.resolve_and_get_model(requested).await {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return error(StatusCode::NOT_FOUND, "Model not found")
        }
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to resolve model"),
    };
    // Do not send a differently resolved model through an Astra-only transport.
    if !model
        .model_name
        .strip_prefix("openai/")
        .is_some_and(inference_providers::responses_raw::is_astra)
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Native stateless Responses requires an Astra model",
        );
    }
    if model.model_name != requested && common::no_aliasing_requested(&headers) {
        return error(
            StatusCode::BAD_REQUEST,
            "Model alias resolution is disabled for this request",
        );
    }
    let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
        Ok(id) => id,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Invalid billing context"),
    };
    let guard = match app
        .completion_service
        .acquire_concurrent_slot(api_key.organization.id.0, model.id, &model.model_name)
        .await
    {
        Ok(guard) => guard,
        Err(services::completions::ports::CompletionError::RateLimitExceeded(_)) => {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "Concurrent request limit exceeded",
            )
        }
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to enforce concurrency limit",
            )
        }
    };
    let alias =
        (requested != model.model_name).then(|| format!("{requested} -> {}", model.model_name));
    body["model"] = json!(model.model_name);
    body["service_tier"] = json!("default"); // Existing Responses Standard-only policy.
    if let Some(limit) = model.max_output_length {
        let requested = body["max_output_tokens"]
            .as_i64()
            .unwrap_or(i64::from(limit));
        body["max_output_tokens"] = json!(requested.min(i64::from(limit)));
    }
    if let Some(prompt) = api_key.organization.settings["system_prompt"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        let instructions = body["instructions"].as_str().unwrap_or("");
        body["instructions"] = json!(if instructions.is_empty() {
            prompt.to_string()
        } else {
            format!("{prompt}\n\n{instructions}")
        });
    }
    let stream = body["stream"] == true;
    let (upstream, attribution) = match app
        .inference_provider_pool
        .responses_raw(&model.model_name, body)
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "Native OpenAI Responses request failed",
            )
        }
    };
    let billing = Billing {
        app,
        request_hash,
        organization_id: api_key.organization.id.0,
        workspace_id: api_key.workspace.id.0,
        api_key_id,
        model_id: model.id,
        inference_type: if stream {
            InferenceType::ChatCompletionStream
        } else {
            InferenceType::ChatCompletion
        },
        attribution,
    };
    respond(upstream, stream, billing, guard, alias).await
}

struct Billing {
    app: AppState,
    request_hash: String,
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    inference_type: InferenceType,
    attribution: ProviderAttribution,
}

#[derive(Default)]
struct Usage {
    id: Option<String>,
    tokens: Option<inference_providers::TokenUsage>,
    tier: Option<String>,
    reason: Option<StopReason>,
    terminal: bool,
}

impl Usage {
    fn apply(&mut self, response: &Value) -> Result<(), &'static str> {
        if let Some(id) = response["id"].as_str() {
            self.id = Some(id.into());
        }
        if let Some(tier) = response["service_tier"].as_str() {
            self.tier = Some(tier.into());
        }
        if let Some(usage) = response.get("usage").filter(|v| !v.is_null()) {
            let count = |k: &str| {
                usage[k]
                    .as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                    .filter(|n| *n >= 0)
                    .ok_or("Invalid OpenAI Responses usage")
            };
            self.tokens = Some(inference_providers::TokenUsage {
                prompt_tokens: count("input_tokens")?,
                completion_tokens: count("output_tokens")?,
                total_tokens: count("total_tokens")?,
                prompt_tokens_details: usage.get("input_tokens_details").cloned(),
            });
        }
        match response["status"].as_str() {
            Some("completed") => {
                self.terminal = true;
                self.reason = Some(StopReason::Completed);
            }
            Some("incomplete") => {
                self.terminal = true;
                self.reason = Some(match response["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => StopReason::Length,
                    Some("content_filter") => StopReason::ContentFilter,
                    _ => StopReason::Incomplete,
                });
            }
            Some("failed" | "cancelled") => {
                self.terminal = true;
                self.reason = Some(StopReason::ProviderError);
            }
            _ => {}
        }
        Ok(())
    }
}

impl Billing {
    async fn finish(
        self,
        usage: Usage,
        reason: StopReason,
        digest: Option<String>,
    ) -> Result<(), ()> {
        let id = usage.id.clone();
        if let Some(tokens) = usage.tokens {
            let request = RecordUsageServiceRequest {
                organization_id: self.organization_id,
                workspace_id: self.workspace_id,
                api_key_id: self.api_key_id,
                model_id: self.model_id,
                input_tokens: tokens.prompt_tokens,
                output_tokens: tokens.completion_tokens,
                cache_read_tokens: tokens.cached_tokens(),
                cache_write: None,
                profiled_cache_write_tokens: tokens.cache_write_tokens(),
                requested_service_tier: Some(TextServiceTier::Default),
                provider_service_tier: usage.tier,
                inference_type: self.inference_type,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: id.as_deref().map(hash_inference_id_to_uuid),
                provider_request_id: id.clone(),
                stop_reason: Some(reason),
                response_id: None,
                image_count: None,
                provider_attribution: self.attribution,
            };
            if let Err(e) = self.app.usage_service.record_usage(request).await {
                tracing::error!(error = %e, "Failed to record native Responses usage");
                return Err(());
            }
        }
        if let (Some(id), Some(digest)) = (id, digest) {
            // Only digest material is retained, never response/input content.
            if let Err(e) = self
                .app
                .attestation_service
                .store_response_signature(&id, self.request_hash, digest)
                .await
            {
                tracing::error!(error = %e, "Failed to record native Responses signature");
            }
        }
        Ok(())
    }
}

fn response(
    status: StatusCode,
    upstream_headers: HeaderMap,
    body: Body,
    alias: Option<String>,
    id: Option<&str>,
) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    for (name, value) in upstream_headers {
        if let Some(name) =
            name.filter(|n| matches!(n.as_str(), "content-type" | "x-request-id" | "retry-after"))
        {
            response.headers_mut().insert(name, value);
        }
    }
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        "x-serving-provider",
        HeaderValue::from_static("non-attested"),
    );
    if let Some(id) = id {
        response.headers_mut().insert(
            HEADER_INFERENCE_ID,
            HeaderValue::from_str(&hash_inference_id_to_uuid(id).to_string()).unwrap(),
        );
    }
    if let Some(alias) = alias.and_then(|s| HeaderValue::from_str(&s).ok()) {
        response
            .headers_mut()
            .insert(common::HEADER_MODEL_ALIAS_RESOLVED, alias);
    }
    response
}

async fn respond(
    mut upstream: ResponsesRawResponse,
    stream: bool,
    billing: Billing,
    guard: ConcurrentRequestGuard,
    alias: Option<String>,
) -> Response {
    if !upstream.status.is_success() {
        // No retries, body rewriting, or charging for rejected requests.
        return response(
            upstream.status,
            upstream.headers,
            Body::from_stream(upstream.body),
            alias,
            None,
        );
    }
    if stream {
        return response(
            upstream.status,
            upstream.headers,
            Body::from_stream(UsageStream {
                inner: upstream.body,
                parser: EventUsage::default(),
                digest: Sha256::new(),
                billing: Some(billing),
                guard: Some(guard),
                runtime: tokio::runtime::Handle::current(),
                done: false,
            }),
            alias,
            None,
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = upstream.body.next().await {
        match chunk {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(_) => return error(StatusCode::BAD_GATEWAY, "OpenAI response was interrupted"),
        }
    }
    let mut usage = Usage::default();
    if serde_json::from_slice::<Value>(&bytes)
        .ok()
        .is_none_or(|v| usage.apply(&v).is_err())
        || !usage.terminal
        || (usage.tokens.is_none() && !matches!(usage.reason, Some(StopReason::ProviderError)))
    {
        return error(
            StatusCode::BAD_GATEWAY,
            "OpenAI response omitted terminal status or usage",
        );
    }
    let id = usage.id.clone();
    let reason = usage.reason.clone().unwrap_or(StopReason::Incomplete);
    if billing
        .finish(usage, reason, Some(hex::encode(Sha256::digest(&bytes))))
        .await
        .is_err()
    {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to record Responses usage",
        );
    }
    response(
        upstream.status,
        upstream.headers,
        Body::from(bytes),
        alias,
        id.as_deref(),
    )
}

// Only the usage observer parses SSE. Client-visible bytes remain untouched.
#[derive(Default)]
struct EventUsage {
    line: Vec<u8>,
    usage: Usage,
}
impl EventUsage {
    fn push(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        for segment in bytes.split_inclusive(|b| *b == b'\n') {
            // Terminal Responses events contain the complete output, not just
            // usage. Bound memory while allowing large tool arguments/output.
            if self.line.len() + segment.len() > 16 * 1024 * 1024 {
                return Err("Responses SSE event exceeded observer limit");
            }
            self.line.extend_from_slice(segment);
            if self.line.last() == Some(&b'\n') {
                self.flush()?;
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), &'static str> {
        let line = std::mem::take(&mut self.line);
        let Some(data) = line.strip_prefix(b"data:") else {
            return Ok(());
        };
        let data = data.trim_ascii();
        if data == b"[DONE]" {
            return Ok(());
        }
        let event: Value =
            serde_json::from_slice(data).map_err(|_| "Invalid Responses SSE JSON")?;
        if let Some(response) = event.get("response") {
            self.usage.apply(response)?;
        }
        if event["type"] == "error" {
            self.usage.terminal = true;
            self.usage.reason = Some(StopReason::ProviderError);
        }
        Ok(())
    }
}

struct UsageStream {
    inner: ResponsesRawBody,
    parser: EventUsage,
    digest: Sha256,
    billing: Option<Billing>,
    guard: Option<ConcurrentRequestGuard>,
    runtime: tokio::runtime::Handle,
    done: bool,
}
impl UsageStream {
    fn finish(&mut self, fallback: StopReason, complete: bool) {
        self.done = true;
        self.guard.take();
        if let Some(billing) = self.billing.take() {
            let usage = std::mem::take(&mut self.parser.usage);
            let reason = if complete {
                usage.reason.clone().unwrap_or(fallback)
            } else {
                fallback
            };
            let digest = complete.then(|| hex::encode(self.digest.clone().finalize()));
            let runtime = self.runtime.clone();
            self.runtime.spawn_blocking(move || {
                runtime.block_on(async move {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        billing.finish(usage, reason, digest),
                    )
                    .await;
                })
            });
        }
    }
}
impl Stream for UsageStream {
    type Item = Result<Bytes, CompletionError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                if let Err(message) = this.parser.push(&bytes) {
                    this.finish(StopReason::ProviderError, false);
                    return Poll::Ready(Some(Err(CompletionError::InvalidResponse(
                        message.into(),
                    ))));
                }
                this.digest.update(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish(StopReason::ProviderError, false);
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                let valid = this.parser.flush().is_ok()
                    && this.parser.usage.terminal
                    && (this.parser.usage.tokens.is_some()
                        || matches!(this.parser.usage.reason, Some(StopReason::ProviderError)));
                this.finish(StopReason::Incomplete, valid);
                if valid {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Err(CompletionError::InvalidResponse(
                        "Responses stream ended without terminal usage".into(),
                    ))))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
impl Drop for UsageStream {
    fn drop(&mut self) {
        self.finish(StopReason::ClientDisconnect, false);
    }
}

#[cfg(test)]
mod tests;
