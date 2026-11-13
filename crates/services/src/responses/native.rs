//! Native stateless Responses orchestration, accounting and stream lifecycle.
use crate::{
    attestation::ports::AttestationServiceTrait,
    completions::{ports::ConcurrentRequestGuard, CompletionServiceTrait},
    inference_provider_pool::InferenceProviderPool,
    models::{ModelWithPricing, ModelsError, ModelsServiceTrait},
    usage::{
        InferenceType, ProviderAttribution, RecordUsageServiceRequest, StopReason, TextServiceTier,
        UsageError, UsageServiceTrait,
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
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Best-effort signature store, after the usage write has returned.
const SIGNATURE_STORE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a request waits for the usage write's verdict. Bounds only the
/// waiter: the write itself keeps running on its own task.
const BILLING_VERDICT_WAIT: Duration = Duration::from_secs(35);
/// Upper bound on a provider response id echoed in a billing failure frame.
const MAX_RESPONSE_ID_LEN: usize = 128;
const BILLING_FAILURE_MESSAGE: &str = "Failed to record usage";

const CLIENT_TOOLS_ERROR: &str = "Native stateless Responses supports valid client-managed function, custom, and namespace tools, plus top-level web_search";

/// Terminal Responses events contain the complete output, not just usage.
/// Bound memory while allowing large tool arguments/output.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
/// Bytes withheld while a terminal event waits for its usage write: the
/// terminal event itself plus any trailing frames before upstream EOF.
const MAX_HELD_BYTES: usize = MAX_EVENT_BYTES + 1024 * 1024;

#[derive(Clone)]
pub struct NativeResponsesService {
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
    BillingOutcome {
        inference_id: Uuid,
        message: &'static str,
    },
}

pub struct NativeResponse {
    pub upstream: ResponsesRawResponse,
    /// Billing key of this request's usage row, known before any byte is
    /// streamed. The provider response id stays the provider request id and
    /// the signature key.
    pub inference_id: Uuid,
}

pub fn selected(canonical_model: &str) -> bool {
    canonical_model.starts_with("openai/")
}

impl NativeResponsesService {
    /// Resolve aliases before choosing the OpenAI-native transport. Other
    /// models retain typed validation and error handling. A model the
    /// caller may not use is never selected: the request takes the legacy
    /// path and gets its unknown-model answer.
    pub async fn selected_model(
        &self,
        body: &Value,
        caller: crate::models::ModelCaller,
    ) -> Result<Option<ModelWithPricing>, ModelsError> {
        let Some(identifier) = body["model"].as_str() else {
            return Ok(None);
        };
        let model = match self
            .models_service
            .resolve_and_get_model(identifier, caller)
            .await
        {
            Ok(model) => model,
            Err(ModelsError::NotFound(_)) => return Ok(None),
            // A failed lookup cannot safely choose a transport. Let the
            // route report the failure instead of retrying through Chat.
            Err(error) => return Err(error),
        };
        Ok(selected(&model.model_name).then_some(model))
    }

    pub async fn execute(
        &self,
        model: ModelWithPricing,
        mut body: Value,
        context: NativeResponsesContext,
    ) -> Result<NativeResponse, NativeResponsesError> {
        model
            .validate_endpoint(crate::models::InferenceEndpoint::Responses)
            .map_err(NativeResponsesError::InvalidRequest)?;
        // Keep transport selection independent of request state: an invalid
        // OpenAI request must never fall back to Chat Completions.
        if !selected(&model.model_name) {
            return Err(NativeResponsesError::InvalidRequest(
                "Native Responses requires an OpenAI model",
            ));
        }
        normalize_stateless_request(&mut body)?;
        if model
            .output_modalities
            .as_ref()
            .is_some_and(|modalities| modalities.iter().any(|modality| modality == "image"))
        {
            return Err(NativeResponsesError::InvalidRequest(
                "Image generation and image editing are not supported by /v1/responses. Use /v1/images/generations or /v1/images/edits.",
            ));
        }
        body = crate::responses::client_tools::normalize_native_request(body)
            .map_err(|_| NativeResponsesError::InvalidRequest(CLIENT_TOOLS_ERROR))?;
        validate(&body).map_err(NativeResponsesError::InvalidRequest)?;
        normalize_max_tool_calls(&mut body).map_err(NativeResponsesError::InvalidRequest)?;
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
            usage_service: self.usage_service.clone(),
            attestation_service: self.attestation_service.clone(),
            inference_id: Uuid::new_v4(),
            request_hash: context.request_hash,
            organization_id: context.organization_id,
            workspace_id: context.workspace_id,
            api_key_id: context.api_key_id,
            model_id: model.id,
            model_name: model.model_name.clone(),
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

/// Match the typed Responses default, while always sending explicit no-store
/// semantics to the native provider. The provider boundary still checks the
/// fully normalized request independently.
fn normalize_stateless_request(body: &mut Value) -> Result<(), NativeResponsesError> {
    let fields = body
        .as_object_mut()
        .ok_or(NativeResponsesError::InvalidRequest(
            "Request must be an object",
        ))?;
    if fields.get("store").is_none_or(Value::is_null) {
        fields.insert("store".into(), json!(false));
    }
    if !inference_providers::responses_raw::is_stateless(body) {
        return Err(NativeResponsesError::InvalidRequest(
            "Responses requires store: false and does not support conversation, previous_response_id, or background execution",
        ));
    }
    Ok(())
}

/// Apply the typed route's hosted tool budget to the provider request:
/// omitted or null uses the default, larger values are clamped, and a present
/// value must be a positive integer.
fn normalize_max_tool_calls(body: &mut Value) -> Result<(), &'static str> {
    use crate::responses::models::{DEFAULT_MAX_HOSTED_TOOL_CALLS, MAX_HOSTED_TOOL_CALLS};
    let fields = body.as_object_mut().ok_or("Request must be an object")?;
    let budget = match fields.get("max_tool_calls") {
        None | Some(Value::Null) => DEFAULT_MAX_HOSTED_TOOL_CALLS,
        Some(value) => match value.as_i64() {
            Some(requested) if requested > 0 => usize::try_from(requested)
                .unwrap_or(MAX_HOSTED_TOOL_CALLS)
                .min(MAX_HOSTED_TOOL_CALLS),
            _ => return Err("max_tool_calls must be a positive integer"),
        },
    };
    fields.insert("max_tool_calls".into(), json!(budget));
    Ok(())
}

struct Billing {
    usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    attestation_service: Arc<dyn AttestationServiceTrait>,
    /// Generated per request; the usage row is written under exactly this id.
    inference_id: Uuid,
    request_hash: String,
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    /// Canonical model name, reported in a billing failure frame.
    model_name: String,
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
    if body.get("prompt").is_some_and(|v| !v.is_null()) {
        return Err("Stored prompt references are unavailable for native stateless Responses");
    }
    // Replay supported native output inline; Cloud file IDs and unsupported
    // tool continuation items cannot be resolved by this stateless transport.
    if let Some(input) = body.get("input") {
        if !input.is_string() && !input.is_array() {
            return Err("input must be a string or array");
        }
        for item in input.as_array().into_iter().flatten() {
            if !matches!(
                item["type"].as_str(),
                None | Some(
                    "message"
                        | "function_call"
                        | "function_call_output"
                        | "custom_tool_call"
                        | "custom_tool_call_output"
                        | "web_search_call"
                        | "reasoning"
                )
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
                ..Default::default()
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

    /// A terminal response that either carries billable usage or is a
    /// provider failure with nothing to bill.
    fn is_complete(&self) -> bool {
        self.terminal
            && (self.tokens.is_some() || matches!(self.reason, Some(StopReason::ProviderError)))
    }
}

/// What the usage write established about the charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BillingVerdict {
    /// The write returned `Ok`, or there was nothing to bill.
    Recorded,
    /// The write failed before reaching the repository, so no row exists.
    Rejected,
    /// The write failed in a way that may follow a committed row.
    Unknown,
}

impl BillingVerdict {
    /// `UsageServiceImpl::record_usage` returns these kinds only while
    /// resolving the model and computing the price, before the repository
    /// write. Every other kind, including `InternalError` (which also wraps
    /// repository errors) and `ReportingTimeout`, may follow a commit.
    fn of_error(error: &UsageError) -> Self {
        match error {
            UsageError::ModelNotFound(_)
            | UsageError::CostCalculationOverflow(_)
            | UsageError::ValidationError(_) => Self::Rejected,
            UsageError::InternalError(_)
            | UsageError::ReportingTimeout
            | UsageError::LimitExceeded(_)
            | UsageError::Unauthorized(_)
            | UsageError::NotFound(_) => Self::Unknown,
        }
    }

    fn error_kind(error: &UsageError) -> &'static str {
        match error {
            UsageError::ModelNotFound(_) => "model_not_found",
            UsageError::CostCalculationOverflow(_) => "cost_overflow",
            UsageError::ValidationError(_) => "validation",
            UsageError::InternalError(_) => "internal",
            UsageError::ReportingTimeout => "timeout",
            UsageError::LimitExceeded(_) => "limit_exceeded",
            UsageError::Unauthorized(_) => "unauthorized",
            UsageError::NotFound(_) => "not_found",
        }
    }
}

/// A started usage write. `recorded` resolves once the write has returned,
/// with its verdict; a write task that ends without returning is dropped
/// unsent and reads as unknown. `done` also covers the best-effort
/// signature store that follows a recorded write.
struct BillingTask {
    recorded: oneshot::Receiver<BillingVerdict>,
    done: tokio::task::JoinHandle<()>,
}

impl Billing {
    /// Run the usage write (then, on success, the signature store) on its own
    /// task, so a client disconnect never cancels a charge in flight. The
    /// write is not wrapped in a client-side timeout: its verdict is the
    /// result the usage service actually returned. Waiters bound only their
    /// own wait (`BILLING_VERDICT_WAIT`) and read a late verdict as unknown.
    fn spawn(
        self,
        runtime: &tokio::runtime::Handle,
        usage: Usage,
        reason: StopReason,
        digest: Option<String>,
    ) -> BillingTask {
        let (verdict, recorded) = oneshot::channel();
        let done = runtime.spawn(async move {
            let outcome = self.record(&usage, reason).await;
            let _ = verdict.send(outcome);
            if outcome == BillingVerdict::Recorded {
                self.store_signature(usage.id, digest).await;
            }
        });
        BillingTask { recorded, done }
    }

    /// The verdict of a single usage write (retries live in the repository
    /// layer); `Recorded` when there is nothing to bill.
    async fn record(&self, usage: &Usage, reason: StopReason) -> BillingVerdict {
        let Some(tokens) = usage.tokens.as_ref() else {
            return BillingVerdict::Recorded;
        };
        let request = RecordUsageServiceRequest {
            input_tokens: tokens.prompt_tokens,
            output_tokens: tokens.completion_tokens,
            cache_read_tokens: tokens.cached_tokens(),
            profiled_cache_write_tokens: tokens.cache_write_tokens(),
            requested_service_tier: Some(TextServiceTier::Default),
            provider_service_tier: usage.tier.clone(),
            // Fresh per request, so the default conflict handling (a
            // repeated insert returns the stored row) keeps repository
            // retries exact.
            inference_id: Some(self.inference_id),
            provider_request_id: usage.id.clone(),
            stop_reason: Some(reason),
            provider_attribution: self.attribution,
            ..RecordUsageServiceRequest::base(
                crate::usage::BillingIdentity {
                    organization_id: self.organization_id,
                    workspace_id: self.workspace_id,
                    api_key_id: self.api_key_id,
                    model_id: self.model_id,
                },
                self.inference_type,
            )
        };
        match self.usage_service.record_usage(request).await {
            Ok(_) => BillingVerdict::Recorded,
            Err(e) => {
                let verdict = BillingVerdict::of_error(&e);
                tracing::error!(
                    inference_id = %self.inference_id,
                    error_kind = BillingVerdict::error_kind(&e),
                    verdict = ?verdict,
                    "Failed to record native Responses usage"
                );
                verdict
            }
        }
    }

    async fn store_signature(self, id: Option<String>, digest: Option<String>) {
        let (Some(id), Some(digest)) = (id, digest) else {
            return;
        };
        // Only digest material is retained, never response/input content.
        let owner = crate::attestation::SignatureOwner {
            organization_id: self.organization_id,
            workspace_id: self.workspace_id,
        };
        match tokio::time::timeout(
            SIGNATURE_STORE_TIMEOUT,
            self.attestation_service.store_response_signature(
                owner,
                &id,
                self.request_hash,
                digest,
            ),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(
                inference_id = %self.inference_id,
                error_kind = e.kind(),
                "Failed to record native Responses signature"
            ),
            Err(_) => tracing::error!(
                inference_id = %self.inference_id,
                error_kind = "signature_store_timeout",
                "Failed to record native Responses signature"
            ),
        }
    }
}

async fn account(
    mut upstream: ResponsesRawResponse,
    stream: bool,
    billing: Billing,
    guard: ConcurrentRequestGuard,
) -> Result<NativeResponse, NativeResponsesError> {
    let inference_id = billing.inference_id;
    if !upstream.status.is_success() {
        return Ok(NativeResponse {
            upstream,
            inference_id,
        });
    }
    let runtime = tokio::runtime::Handle::current();
    if stream {
        upstream.body = Box::pin(UsageStream::new(upstream.body, billing, guard, runtime));
        return Ok(NativeResponse {
            upstream,
            inference_id,
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = upstream.body.next().await {
        bytes.extend_from_slice(
            &chunk
                .map_err(|_| NativeResponsesError::Provider("OpenAI response was interrupted"))?,
        );
    }
    // The provider call is over; the usage write does not hold the slot.
    drop(guard);
    let mut usage = Usage::default();
    if serde_json::from_slice::<Value>(&bytes)
        .ok()
        .is_none_or(|v| usage.apply(&v).is_err())
        || !usage.is_complete()
    {
        return Err(NativeResponsesError::Provider(
            "OpenAI response omitted terminal status or usage",
        ));
    }
    let reason = usage.reason.clone().unwrap_or(StopReason::Incomplete);
    let digest = hex::encode(Sha256::digest(&bytes));
    // The task outlives this handler, so a disconnect cannot cancel the write.
    let task = billing.spawn(&runtime, usage, reason, Some(digest));
    let verdict = match tokio::time::timeout(BILLING_VERDICT_WAIT, task.recorded).await {
        Ok(verdict) => verdict.unwrap_or(BillingVerdict::Unknown),
        Err(_) => {
            log_verdict_wait_timeout(inference_id);
            BillingVerdict::Unknown
        }
    };
    match verdict {
        BillingVerdict::Recorded => {}
        BillingVerdict::Rejected => {
            return Err(NativeResponsesError::BillingOutcome {
                inference_id,
                message: "Failed to record Responses usage",
            });
        }
        // The row may exist: report the billing id without claiming a
        // failed charge.
        BillingVerdict::Unknown => {
            return Err(NativeResponsesError::BillingOutcome {
                inference_id,
                message: "Responses usage outcome is unavailable",
            });
        }
    }
    // Keep the signature available as soon as the response is returned.
    let _ = task.done.await;
    upstream.body = Box::pin(futures_util::stream::once(
        async move { Ok(Bytes::from(bytes)) },
    ));
    Ok(NativeResponse {
        upstream,
        inference_id,
    })
}

fn log_verdict_wait_timeout(inference_id: Uuid) {
    tracing::warn!(
        inference_id = %inference_id,
        error_kind = "usage_wait_timeout",
        "Native Responses usage outcome not known in time"
    );
}

/// Observes Responses SSE for usage while keeping client-visible bytes
/// untouched. Lines of an ordinary event are released once its `data:` line
/// is parsed (or at its blank separator). From the terminal event on, every
/// byte is held until the usage write decides whether it may be sent.
#[derive(Default)]
struct EventUsage {
    line: Vec<u8>,
    /// Complete lines not yet released: the current event's leading lines,
    /// or everything since the terminal event.
    pending: Vec<u8>,
    usage: Usage,
    /// Sequence number of the terminal event, which a failure frame replaces.
    terminal_sequence: Option<u64>,
    /// `created_at` of the observed response, when it is an integer.
    created_at: Option<i64>,
}
impl EventUsage {
    /// Observe upstream bytes, appending those safe to forward to `out`.
    fn push(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), &'static str> {
        for segment in bytes.split_inclusive(|b| *b == b'\n') {
            if self.line.len() + segment.len() > MAX_EVENT_BYTES {
                return Err("Responses SSE event exceeded observer limit");
            }
            self.line.extend_from_slice(segment);
            if self.line.last() == Some(&b'\n') {
                self.end_line(out)?;
            }
        }
        if self.pending.len() > MAX_HELD_BYTES {
            return Err("Responses SSE event exceeded observer limit");
        }
        Ok(())
    }

    /// Classify a trailing line without a newline at upstream EOF.
    fn finish_input(&mut self, out: &mut Vec<u8>) -> Result<(), &'static str> {
        if self.line.is_empty() {
            return Ok(());
        }
        self.end_line(out)
    }

    fn end_line(&mut self, out: &mut Vec<u8>) -> Result<(), &'static str> {
        let line = std::mem::take(&mut self.line);
        let is_data = match line.strip_prefix(b"data:") {
            Some(data) => {
                self.observe(data.trim_ascii())?;
                true
            }
            None => false,
        };
        let is_blank = line.trim_ascii().is_empty();
        self.pending.extend_from_slice(&line);
        // `terminal` is sticky: once a terminal event is seen, nothing more
        // is released here.
        if !self.usage.terminal && (is_data || is_blank) {
            out.append(&mut self.pending);
        }
        Ok(())
    }

    fn observe(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data == b"[DONE]" {
            return Ok(());
        }
        let event: Value =
            serde_json::from_slice(data).map_err(|_| "Invalid Responses SSE JSON")?;
        let was_terminal = self.usage.terminal;
        if let Some(response) = event.get("response") {
            self.usage.apply(response)?;
            if let Some(created_at) = response["created_at"].as_i64() {
                self.created_at = Some(created_at);
            }
        }
        if event["type"] == "error" {
            self.usage.terminal = true;
            self.usage.reason = Some(StopReason::ProviderError);
        }
        if !was_terminal && self.usage.terminal {
            self.terminal_sequence = event["sequence_number"].as_u64();
        }
        Ok(())
    }

    /// The frame sent instead of the withheld terminal event if its usage
    /// write fails. Built before the observed usage moves to the write.
    fn billing_failure_event(&self, inference_id: Uuid, model: &str) -> Bytes {
        billing_failure_event(&FailureFrame {
            response_id: self.usage.id.as_deref(),
            inference_id,
            created_at: self.created_at,
            model,
            tokens: self.usage.tokens.as_ref(),
            sequence: self.terminal_sequence,
        })
    }
}

/// Whitelisted inputs of a billing failure frame: ids, a timestamp, the
/// canonical model name, token counts, and the replaced sequence number.
struct FailureFrame<'a> {
    response_id: Option<&'a str>,
    inference_id: Uuid,
    created_at: Option<i64>,
    model: &'a str,
    tokens: Option<&'a inference_providers::TokenUsage>,
    sequence: Option<u64>,
}

/// The id the client already saw in earlier events, when it is a plain
/// bounded token; otherwise a value derived from the billing id.
fn failure_response_id(observed: Option<&str>, inference_id: Uuid) -> String {
    observed
        .filter(|id| {
            !id.is_empty()
                && id.len() <= MAX_RESPONSE_ID_LEN
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .map_or_else(|| format!("resp_{}", inference_id.simple()), str::to_owned)
}

/// A complete `response.failed` event that replaces the withheld terminal
/// event (and so takes its sequence number). Carries no output, request
/// parameters, metadata, or upstream error text.
fn billing_failure_event(frame: &FailureFrame<'_>) -> Bytes {
    let (input_tokens, output_tokens, total_tokens) = frame.tokens.map_or((0, 0, 0), |t| {
        (t.prompt_tokens, t.completion_tokens, t.total_tokens)
    });
    let mut event = json!({
        "type": "response.failed",
        "response": {
            "id": failure_response_id(frame.response_id, frame.inference_id),
            "object": "response",
            "created_at": frame.created_at.unwrap_or_else(|| chrono::Utc::now().timestamp()),
            "status": "failed",
            "background": false,
            "error": {
                "type": "server_error",
                "code": "server_error",
                "message": BILLING_FAILURE_MESSAGE,
                "param": null,
            },
            "incomplete_details": null,
            "instructions": null,
            "max_output_tokens": null,
            "max_tool_calls": null,
            "model": frame.model,
            "output": [],
            "parallel_tool_calls": true,
            "previous_response_id": null,
            "reasoning": null,
            "service_tier": "default",
            "store": false,
            "temperature": 1.0,
            "tool_choice": "auto",
            "tools": [],
            "top_logprobs": 0,
            "top_p": 1.0,
            "truncation": "disabled",
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": total_tokens,
            },
            "user": null,
            "metadata": {},
        },
    });
    if let Some(sequence) = frame.sequence {
        event["sequence_number"] = json!(sequence);
    }
    Bytes::from(format!("event: response.failed\ndata: {event}\n\n"))
}

enum StreamState {
    Streaming,
    /// Upstream ended with a terminal event; `held` waits for the write's
    /// verdict, and `failure` replaces it only if the write was rejected
    /// before reaching the repository. `deadline` bounds the wait only.
    Billing {
        recorded: oneshot::Receiver<BillingVerdict>,
        deadline: Pin<Box<tokio::time::Sleep>>,
        inference_id: Uuid,
        held: Bytes,
        failure: Bytes,
    },
    Done,
}

struct UsageStream {
    inner: ResponsesRawBody,
    parser: EventUsage,
    digest: Sha256,
    /// Taken exactly once: by the terminal write, or by an abandoned stream.
    billing: Option<Billing>,
    guard: Option<ConcurrentRequestGuard>,
    runtime: tokio::runtime::Handle,
    state: StreamState,
}
impl UsageStream {
    fn new(
        inner: ResponsesRawBody,
        billing: Billing,
        guard: ConcurrentRequestGuard,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            inner,
            parser: EventUsage::default(),
            digest: Sha256::new(),
            billing: Some(billing),
            guard: Some(guard),
            runtime,
            state: StreamState::Streaming,
        }
    }

    /// Record whatever usage was observed when the stream cannot complete
    /// (upstream error, invalid stream, or client disconnect). Detached: no
    /// client is waiting for the verdict.
    fn abandon(&mut self, reason: StopReason) {
        self.state = StreamState::Done;
        self.guard.take();
        if let Some(billing) = self.billing.take() {
            let usage = std::mem::take(&mut self.parser.usage);
            let _ = billing.spawn(&self.runtime, usage, reason, None);
        }
    }

    /// Upstream EOF: either start the gating usage write for the held
    /// terminal event, or fail the stream.
    fn end_of_input(&mut self) -> Option<Result<Bytes, CompletionError>> {
        let mut held = Vec::new();
        let parsed = self.parser.finish_input(&mut held).is_ok();
        if !parsed || !self.parser.usage.is_complete() {
            self.abandon(StopReason::Incomplete);
            return Some(Err(CompletionError::InvalidResponse(
                "Responses stream ended without terminal usage".into(),
            )));
        }
        held.append(&mut self.parser.pending);
        let held = Bytes::from(held);
        self.guard.take();
        let Some(billing) = self.billing.take() else {
            self.state = StreamState::Done;
            return Some(Err(CompletionError::InvalidResponse(
                "Responses stream billing state was lost".into(),
            )));
        };
        let inference_id = billing.inference_id;
        let failure = self
            .parser
            .billing_failure_event(inference_id, &billing.model_name);
        let usage = std::mem::take(&mut self.parser.usage);
        let needs_write = usage.tokens.is_some();
        let reason = usage.reason.clone().unwrap_or(StopReason::Incomplete);
        let digest = hex::encode(self.digest.clone().finalize());
        let task = billing.spawn(&self.runtime, usage, reason, Some(digest));
        if needs_write {
            // Dropping `task.done` detaches the write: it keeps running if
            // the client disconnects while waiting.
            self.state = StreamState::Billing {
                recorded: task.recorded,
                deadline: Box::pin(tokio::time::sleep(BILLING_VERDICT_WAIT)),
                inference_id,
                held,
                failure,
            };
            None
        } else {
            // A provider failure with nothing to bill keeps its own frame.
            self.state = StreamState::Done;
            Some(Ok(held))
        }
    }
}
impl Stream for UsageStream {
    type Item = Result<Bytes, CompletionError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                StreamState::Streaming => match this.inner.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(bytes))) => {
                        let mut out = Vec::new();
                        if let Err(message) = this.parser.push(&bytes, &mut out) {
                            this.abandon(StopReason::ProviderError);
                            return Poll::Ready(Some(Err(CompletionError::InvalidResponse(
                                message.into(),
                            ))));
                        }
                        this.digest.update(&bytes);
                        if !out.is_empty() {
                            return Poll::Ready(Some(Ok(Bytes::from(out))));
                        }
                    }
                    Poll::Ready(Some(Err(e))) => {
                        this.abandon(StopReason::ProviderError);
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Ready(None) => {
                        if let Some(item) = this.end_of_input() {
                            return Poll::Ready(Some(item));
                        }
                    }
                    Poll::Pending => return Poll::Pending,
                },
                StreamState::Billing {
                    recorded,
                    deadline,
                    inference_id,
                    held,
                    failure,
                } => {
                    let verdict = match Pin::new(recorded).poll(cx) {
                        Poll::Ready(verdict) => verdict.unwrap_or(BillingVerdict::Unknown),
                        Poll::Pending => match deadline.as_mut().poll(cx) {
                            // Stop waiting; the detached write continues.
                            Poll::Ready(()) => {
                                log_verdict_wait_timeout(*inference_id);
                                BillingVerdict::Unknown
                            }
                            Poll::Pending => return Poll::Pending,
                        },
                    };
                    let item = match verdict {
                        BillingVerdict::Recorded => Ok(std::mem::take(held)),
                        BillingVerdict::Rejected => Ok(std::mem::take(failure)),
                        // The row may exist, so neither a success nor a
                        // failure frame: end the body with a transport error.
                        BillingVerdict::Unknown => Err(CompletionError::InvalidResponse(
                            "Responses usage outcome is unavailable".into(),
                        )),
                    };
                    this.state = StreamState::Done;
                    return Poll::Ready(Some(item));
                }
                StreamState::Done => return Poll::Ready(None),
            }
        }
    }
}
impl Drop for UsageStream {
    fn drop(&mut self) {
        // Before EOF this records observed usage once. A terminal write that
        // already started keeps running on its own task.
        self.abandon(StopReason::ClientDisconnect);
    }
}

#[cfg(test)]
mod tests;
