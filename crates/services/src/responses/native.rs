//! Native stateless Responses orchestration, accounting and stream lifecycle.
use crate::{
    attestation::ports::AttestationServiceTrait,
    completions::{
        hash_inference_id_to_uuid, ports::ConcurrentRequestGuard, CompletionServiceTrait,
    },
    inference_provider_pool::InferenceProviderPool,
    models::{ModelWithPricing, ModelsServiceTrait},
    usage::{
        InferenceType, ProviderAttribution, RecordUsageServiceRequest, StopReason, TextServiceTier,
        UsageServiceTrait,
    },
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use inference_providers::{
    responses_raw::{ResponsesRawBody, ResponsesRawResponse},
    CompletionError,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use uuid::Uuid;

#[derive(Clone)]
pub struct NativeResponsesService {
    pub models: Vec<String>,
    pub models_service: Arc<dyn ModelsServiceTrait>,
    pub completion_service: Arc<dyn CompletionServiceTrait>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    pub attestation_service: Arc<dyn AttestationServiceTrait>,
}

pub struct NativeResponsesContext {
    pub request_hash: String,
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub fallback_enabled: bool,
    pub system_prompt: Option<String>,
}

#[derive(Debug)]
pub enum NativeResponsesError {
    InvalidRequest(&'static str),
    RateLimited,
    Provider(&'static str),
    Internal(&'static str),
}

pub struct NativeResponse {
    pub upstream: ResponsesRawResponse,
    pub provider_response_id: Option<String>,
}

pub fn selected(models: &[String], canonical_model: &str, body: &Value) -> bool {
    models.iter().any(|model| model == canonical_model)
        && inference_providers::responses_raw::is_stateless(body)
}

impl NativeResponsesService {
    /// Resolve aliases before checking the canonical allowlist. Non-selected
    /// requests retain legacy validation and error handling.
    pub async fn selected_model(&self, body: &Value) -> Option<ModelWithPricing> {
        if self.models.is_empty() || !inference_providers::responses_raw::is_stateless(body) {
            return None;
        }
        let model = self
            .models_service
            .resolve_and_get_model(body["model"].as_str()?)
            .await
            .ok()?;
        selected(&self.models, &model.model_name, body).then_some(model)
    }

    pub async fn execute(
        &self,
        model: ModelWithPricing,
        mut body: Value,
        context: NativeResponsesContext,
    ) -> Result<NativeResponse, NativeResponsesError> {
        // Recheck the allowlist at the service boundary, independent of HTTP routing.
        if !selected(&self.models, &model.model_name, &body) {
            return Err(NativeResponsesError::InvalidRequest(
                "Native Responses is not enabled for this stateless request",
            ));
        }
        validate(&body).map_err(NativeResponsesError::InvalidRequest)?;
        let guard = self
            .completion_service
            .acquire_concurrent_slot(context.organization_id, model.id, &model.model_name)
            .await
            .map_err(|e| match e {
                crate::completions::ports::CompletionError::RateLimitExceeded(_) => {
                    NativeResponsesError::RateLimited
                }
                _ => NativeResponsesError::Internal("Failed to enforce concurrency limit"),
            })?;
        // Scheduling priority is internal NEAR metadata, not an OpenAI parameter.
        // Match the external Chat transport's client-priority stripping policy.
        if let Some(fields) = body.as_object_mut() {
            fields.remove("priority");
            fields.remove("request_priority");
        }
        body["model"] = json!(model.model_name);
        body["service_tier"] = json!("default");
        if let Some(limit) = model.max_output_length {
            let requested = body["max_output_tokens"]
                .as_i64()
                .unwrap_or(i64::from(limit));
            body["max_output_tokens"] = json!(requested.min(i64::from(limit)));
        }
        if let Some(prompt) = context.system_prompt.as_deref().filter(|s| !s.is_empty()) {
            let instructions = body["instructions"].as_str().unwrap_or("");
            body["instructions"] = json!(if instructions.is_empty() {
                prompt.to_string()
            } else {
                format!("{prompt}\n\n{instructions}")
            });
        }
        let stream = body["stream"] == true;
        let (upstream, attribution) = self
            .inference_provider_pool
            .responses_raw(&model.model_name, body, context.fallback_enabled)
            .await
            .map_err(|_| {
                NativeResponsesError::Provider("Native OpenAI Responses request failed")
            })?;
        let billing = Billing {
            service: self.clone(),
            request_hash: context.request_hash,
            organization_id: context.organization_id,
            workspace_id: context.workspace_id,
            api_key_id: context.api_key_id,
            model_id: model.id,
            inference_type: if stream {
                InferenceType::ChatCompletionStream
            } else {
                InferenceType::ChatCompletion
            },
            attribution,
        };
        account(upstream, stream, billing, guard).await
    }
}

struct Billing {
    service: NativeResponsesService,
    request_hash: String,
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    inference_type: InferenceType,
    attribution: ProviderAttribution,
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
            return Err("Native stateless Responses supports client-managed function tools only");
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
                return Err("Unsupported input item for native stateless Responses");
            }
            if ["content", "output"]
                .into_iter()
                .filter_map(|field| item[field].as_array())
                .flatten()
                .any(|p| p["type"] == "input_file" || p.get("file_id").is_some())
            {
                return Err("File references are not supported by native stateless Responses");
            }
        }
    }
    Ok(())
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
            if let Err(e) = self.service.usage_service.record_usage(request).await {
                tracing::error!(error = %e, "Failed to record native Responses usage");
                return Err(());
            }
        }
        if let (Some(id), Some(digest)) = (id, digest) {
            // Only digest material is retained, never response/input content.
            if let Err(e) = self
                .service
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

async fn account(
    mut upstream: ResponsesRawResponse,
    stream: bool,
    billing: Billing,
    guard: ConcurrentRequestGuard,
) -> Result<NativeResponse, NativeResponsesError> {
    if !upstream.status.is_success() {
        return Ok(NativeResponse {
            upstream,
            provider_response_id: None,
        });
    }
    if stream {
        upstream.body = Box::pin(UsageStream {
            inner: upstream.body,
            parser: EventUsage::default(),
            digest: Sha256::new(),
            billing: Some(billing),
            guard: Some(guard),
            runtime: tokio::runtime::Handle::current(),
            done: false,
        });
        return Ok(NativeResponse {
            upstream,
            provider_response_id: None,
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = upstream.body.next().await {
        bytes.extend_from_slice(
            &chunk
                .map_err(|_| NativeResponsesError::Provider("OpenAI response was interrupted"))?,
        );
    }
    let mut usage = Usage::default();
    if serde_json::from_slice::<Value>(&bytes)
        .ok()
        .is_none_or(|v| usage.apply(&v).is_err())
        || !usage.terminal
        || (usage.tokens.is_none() && !matches!(usage.reason, Some(StopReason::ProviderError)))
    {
        return Err(NativeResponsesError::Provider(
            "OpenAI response omitted terminal status or usage",
        ));
    }
    let provider_response_id = usage.id.clone();
    let reason = usage.reason.clone().unwrap_or(StopReason::Incomplete);
    billing
        .finish(usage, reason, Some(hex::encode(Sha256::digest(&bytes))))
        .await
        .map_err(|_| NativeResponsesError::Internal("Failed to record Responses usage"))?;
    upstream.body = Box::pin(futures_util::stream::once(
        async move { Ok(Bytes::from(bytes)) },
    ));
    Ok(NativeResponse {
        upstream,
        provider_response_id,
    })
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
