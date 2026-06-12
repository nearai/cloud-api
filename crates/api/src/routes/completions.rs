use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash},
    models::*,
    routes::{
        api::AppState,
        common::{
            alias_warning_message, inject_warning_field, map_domain_error_to_status,
            no_aliasing_requested, HEADER_MODEL_ALIAS_RESOLVED, HEADER_NO_ALIASING,
        },
        extractors::OpenAiJson,
        files::MAX_FILE_SIZE,
    },
};
use axum::{
    body::{Body, Bytes},
    extract::{Extension, Multipart, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json as ResponseJson, Response},
};
use futures::stream::StreamExt;
use services::auto_redact::{self, AutoRedactError, RedactionMap, StreamUnredact};
use services::common::encryption_headers as service_encryption_headers;
use services::completions::{
    hash_inference_id_to_uuid,
    ports::{CompletionMessage, CompletionRequest as ServiceCompletionRequest},
};
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, Instrument};
use utoipa;
use uuid::Uuid;

// Timeout for synchronous usage recording before response is returned
const USAGE_RECORDING_TIMEOUT_SECS: u64 = 5;

// Upper bound on leading SSE control events (keepalive comments, blank
// lines) consumed while peeking for the first parsed chunk. Real upstreams
// emit zero before the first data chunk; the cap stops a misbehaving or
// malicious upstream from stalling response start or growing the stash
// unbounded. Past the cap we proceed without an Inference-Id header and let
// the remaining stream (including the buffered control events) flow through.
const MAX_LEADING_CONTROL_EVENTS: usize = 32;
const STREAM_SIGNATURE_STORE_TIMEOUT_SECS: u64 = 5;

/// Insert validated E2EE headers into a provider `extra` HashMap.
fn insert_encryption_headers(
    encryption_headers: &crate::routes::common::EncryptionHeaders,
    extra: &mut std::collections::HashMap<String, serde_json::Value>,
) {
    if let Some(ref signing_algo) = encryption_headers.signing_algo {
        extra.insert(
            service_encryption_headers::SIGNING_ALGO.to_string(),
            serde_json::Value::String(signing_algo.clone()),
        );
    }
    if let Some(ref client_pub_key) = encryption_headers.client_pub_key {
        extra.insert(
            service_encryption_headers::CLIENT_PUB_KEY.to_string(),
            serde_json::Value::String(client_pub_key.clone()),
        );
    }
    if let Some(ref model_pub_key) = encryption_headers.model_pub_key {
        extra.insert(
            service_encryption_headers::MODEL_PUB_KEY.to_string(),
            serde_json::Value::String(model_pub_key.clone()),
        );
    }
    if let Some(ref encryption_version) = encryption_headers.encryption_version {
        extra.insert(
            service_encryption_headers::ENCRYPTION_VERSION.to_string(),
            serde_json::Value::String(encryption_version.clone()),
        );
    }
    if let Some(ref encrypt_all_fields) = encryption_headers.encrypt_all_fields {
        extra.insert(
            service_encryption_headers::ENCRYPT_ALL_FIELDS.to_string(),
            serde_json::Value::String(encrypt_all_fields.clone()),
        );
    }
}

// Custom header for exposing the inference ID as a UUID
const HEADER_INFERENCE_ID: &str = "Inference-Id";

/// True when any E2EE encryption header was supplied. E2EE bodies are opaque
/// to the gateway, so alias warnings can't be injected into them — the
/// `x-model-alias-resolved` response header is the only signal in that mode.
fn e2ee_requested(encryption_headers: &crate::routes::common::EncryptionHeaders) -> bool {
    encryption_headers.signing_algo.is_some()
        || encryption_headers.client_pub_key.is_some()
        || encryption_headers.model_pub_key.is_some()
        || encryption_headers.encryption_version.is_some()
        || encryption_headers.encrypt_all_fields.is_some()
}

/// Enforce the `x-no-aliasing` strict mode (issue #573): if the client set
/// the header and the requested model is an alias, refuse with 400 before
/// any inference happens — i.e. before tokens are billed and, for E2EE
/// clients, before a payload is bound to a different model TD's signing key.
///
/// Unknown models fall through (`Ok`) so the completion service returns its
/// canonical "not a valid model name or alias" error.
async fn reject_if_aliased(
    models_service: &Arc<dyn services::models::ModelsServiceTrait>,
    headers: &header::HeaderMap,
    requested_model: &str,
) -> Result<(), Response> {
    if !no_aliasing_requested(headers) {
        return Ok(());
    }
    match models_service.resolve_and_get_model(requested_model).await {
        Ok(m) if m.model_name != requested_model => Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                format!(
                    "Model '{requested_model}' is an alias of '{}' and the request set \
                     {HEADER_NO_ALIASING}. Use the canonical model name '{}'.",
                    m.model_name, m.model_name
                ),
                "model_alias_rejected".to_string(),
            )),
        )
            .into_response()),
        Ok(_) => Ok(()),
        Err(services::models::ModelsError::NotFound(_)) => Ok(()),
        Err(_) => {
            // Strict mode must fail closed: if the catalog can't be
            // consulted, we can't guarantee no alias was applied.
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model for x-no-aliasing check".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
                .into_response())
        }
    }
}

/// Build a `RecordUsageServiceRequest` for image operations (generation or editing).
fn build_image_usage_request(
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    provider_request_id: &str,
    image_count: i32,
    inference_type: services::usage::InferenceType,
) -> services::usage::RecordUsageServiceRequest {
    services::usage::RecordUsageServiceRequest {
        organization_id,
        workspace_id,
        api_key_id,
        model_id,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        inference_type,
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(hash_inference_id_to_uuid(provider_request_id)),
        provider_request_id: Some(provider_request_id.to_string()),
        stop_reason: Some(services::usage::StopReason::Completed),
        response_id: None,
        image_count: Some(image_count),
    }
}

/// Record usage synchronously with timeout, falling back to async retry.
/// Used for non-streaming operations (image gen/edit) where usage should be
/// persisted before the HTTP response is returned.
async fn record_usage_with_sync_fallback(
    usage_service: Arc<dyn services::usage::UsageServiceTrait + Send + Sync>,
    request: services::usage::RecordUsageServiceRequest,
    operation_label: &str,
) {
    let provider_request_id = request.provider_request_id.clone().unwrap_or_default();
    let organization_id = request.organization_id;
    let timeout_duration = Duration::from_secs(USAGE_RECORDING_TIMEOUT_SECS);

    match tokio::time::timeout(
        timeout_duration,
        usage_service.record_usage(request.clone()),
    )
    .await
    {
        Ok(Ok(_)) => {
            tracing::debug!(
                image_id = %provider_request_id,
                %organization_id,
                "{operation_label} usage recorded synchronously"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(
                error = %e,
                image_id = %provider_request_id,
                "Failed to record usage synchronously, retrying async"
            );
            spawn_async_usage_retry(usage_service, request, provider_request_id, operation_label);
        }
        Err(_timeout) => {
            tracing::warn!(
                image_id = %provider_request_id,
                "Usage recording timed out ({USAGE_RECORDING_TIMEOUT_SECS}s), retrying async"
            );
            spawn_async_usage_retry(usage_service, request, provider_request_id, operation_label);
        }
    }
}

/// Spawn an async retry for usage recording after a sync attempt fails or times out.
fn spawn_async_usage_retry(
    usage_service: Arc<dyn services::usage::UsageServiceTrait + Send + Sync>,
    request: services::usage::RecordUsageServiceRequest,
    provider_request_id: String,
    operation_label: &str,
) {
    let label = operation_label.to_string();
    tokio::spawn(async move {
        if let Err(e) = usage_service.record_usage(request).await {
            tracing::error!(
                error = %e,
                image_id = %provider_request_id,
                "Failed to record {label} usage in async retry"
            );
        }
    });
}

// Helper function to provide detailed error context for multipart parsing failures
fn analyze_multipart_error(e: &axum::extract::multipart::MultipartError) -> (StatusCode, String) {
    let error_str = e.to_string();

    // Match against known multipart error patterns to provide specific guidance
    let (status, message) = if error_str.contains("boundary") {
        (
            StatusCode::BAD_REQUEST,
            "Invalid Content-Type boundary in multipart request. Ensure the boundary parameter in the Content-Type header matches the actual boundary markers in the message body.".to_string(),
        )
    } else if error_str.contains("unexpected end") || error_str.contains("unexpected EOF") {
        (
            StatusCode::BAD_REQUEST,
            "Request body ended unexpectedly. The multipart message may be truncated or missing the final boundary marker.".to_string(),
        )
    } else if error_str.contains("field") && error_str.contains("size") {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Individual form field size exceeded. A single field in the multipart request is too large.".to_string(),
        )
    } else if error_str.contains("field") {
        (
            StatusCode::BAD_REQUEST,
            "Invalid form field in multipart request. Check field encoding and format.".to_string(),
        )
    } else if error_str.contains("content") {
        (
            StatusCode::BAD_REQUEST,
            "Invalid Content-Type or content encoding in multipart request.".to_string(),
        )
    } else {
        (
            StatusCode::BAD_REQUEST,
            "Invalid multipart form data. The request does not conform to multipart/form-data format.".to_string(),
        )
    };

    (status, message)
}

/// Returns a safe-to-log category string for a stream-level completion error.
fn completion_stream_error_category(e: &inference_providers::CompletionError) -> &'static str {
    match e {
        inference_providers::CompletionError::CompletionError(_) => "completion_error",
        inference_providers::CompletionError::HttpError { .. } => "http_error",
        inference_providers::CompletionError::InvalidResponse(_) => "invalid_response",
        inference_providers::CompletionError::NoPubKeyProvider(_) => "stale_pubkey",
        inference_providers::CompletionError::Unknown(_) => "unknown",
        inference_providers::CompletionError::ClientMediaError(_) => "client_media_error",
        inference_providers::CompletionError::Timeout { .. } => "timeout",
    }
}

/// Returns an OpenAI-compatible `error.type` for a stream-level completion error.
/// Used in the `data: {"error":{...}}` SSE frame so clients can branch on the type.
///
/// Match is intentionally exhaustive (no `_` arm): a new `CompletionError`
/// variant must force a compile error here so the OpenAI mapping is reviewed
/// explicitly instead of silently defaulting to `server_error`.
fn completion_stream_error_openai_type(e: &inference_providers::CompletionError) -> &'static str {
    match e {
        inference_providers::CompletionError::HttpError { status_code, .. } => match *status_code {
            429 => "rate_limit_exceeded",
            400..=499 => "invalid_request_error",
            _ => "server_error",
        },
        // Client supplied an unfetchable/undecodable image or video: a 400-class
        // bad-input error, surfaced to the client as invalid_request_error to
        // match the non-stream path (map_provider_error -> InvalidParams -> 400).
        inference_providers::CompletionError::ClientMediaError(_) => "invalid_request_error",
        inference_providers::CompletionError::CompletionError(_)
        | inference_providers::CompletionError::InvalidResponse(_)
        | inference_providers::CompletionError::Unknown(_)
        | inference_providers::CompletionError::NoPubKeyProvider(_)
        | inference_providers::CompletionError::Timeout { .. } => "server_error",
    }
}

/// Build an OpenAI-compatible SSE error frame.
///
/// Format: `data: {"error":{"message":"...","type":"..."}}\n\n`. Replaces the
/// historical `data: error: <msg>\n\n` shape that was not valid JSON and broke
/// clients (opencode, vercel/ai-sdk) parsing the `data:` payload as JSON.
fn sse_error_frame(e: &inference_providers::CompletionError) -> Bytes {
    let payload = serde_json::json!({
        "error": {
            "message": e.to_string(),
            "type": completion_stream_error_openai_type(e),
        }
    });
    Bytes::from(format!("data: {payload}\n\n"))
}

fn chat_stream_options(
    request: &ChatCompletionRequest,
) -> Option<inference_providers::models::StreamOptions> {
    request
        .extra
        .get("stream_options")
        .cloned()
        .and_then(|stream_options| {
            serde_json::from_value::<inference_providers::models::StreamOptions>(stream_options)
                .ok()
        })
}

fn chat_stream_include_usage_requested(request: &ChatCompletionRequest) -> bool {
    chat_stream_options(request)
        .and_then(|stream_options| stream_options.include_usage)
        .unwrap_or(false)
}

fn chat_stream_continuous_usage_requested(request: &ChatCompletionRequest) -> bool {
    chat_stream_options(request)
        .and_then(|stream_options| stream_options.continuous_usage_stats)
        .unwrap_or(false)
}

fn chat_stream_has_non_text_modalities(request: &ChatCompletionRequest) -> bool {
    request
        .extra
        .get("modalities")
        .and_then(|modalities| serde_json::from_value::<Vec<String>>(modalities.clone()).ok())
        .is_some_and(|modalities| {
            modalities
                .iter()
                .any(|modality| !modality.eq_ignore_ascii_case("text"))
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChatStreamUsageMode {
    rewrite_public_stream_usage: bool,
    gateway_signature_enabled: bool,
}

fn chat_stream_usage_mode(
    request: &ChatCompletionRequest,
    model_attestation_supported: Option<bool>,
    e2ee_active: bool,
) -> ChatStreamUsageMode {
    let rewrite_public_stream_usage = request.stream == Some(true)
        && chat_stream_include_usage_requested(request)
        && !chat_stream_continuous_usage_requested(request)
        && model_attestation_supported.is_some()
        && !e2ee_active
        && !chat_stream_has_non_text_modalities(request);

    ChatStreamUsageMode {
        rewrite_public_stream_usage,
        gateway_signature_enabled: rewrite_public_stream_usage
            && model_attestation_supported.unwrap_or(false),
    }
}

#[cfg(test)]
fn prepare_chat_stream_chunk_for_client(
    chunk: &mut inference_providers::models::ChatCompletionChunk,
    include_usage: bool,
) -> bool {
    let mut final_usage = None;
    prepare_chat_stream_chunk_for_client_with_state(chunk, include_usage, &mut final_usage)
}

fn prepare_chat_stream_chunk_for_client_with_state(
    chunk: &mut inference_providers::models::ChatCompletionChunk,
    include_usage: bool,
    final_usage: &mut Option<inference_providers::models::TokenUsage>,
) -> bool {
    let is_usage_only_chunk = chunk.choices.is_empty() && chunk.usage.is_some();
    if !include_usage && is_usage_only_chunk {
        return false;
    }

    if include_usage {
        if let Some(usage) = chunk.usage.take() {
            *final_usage = Some(usage);
        }
    }

    if !include_usage {
        chunk.usage = None;
        return true;
    }

    if is_usage_only_chunk {
        return false;
    }

    true
}

fn prepare_stream_chunk_for_client(
    chunk: &mut inference_providers::StreamChunk,
    include_usage: bool,
    final_usage: &mut Option<inference_providers::models::TokenUsage>,
) -> bool {
    match chunk {
        inference_providers::StreamChunk::Chat(chat) => {
            prepare_chat_stream_chunk_for_client_with_state(chat, include_usage, final_usage)
        }
        inference_providers::StreamChunk::Text(_) => true,
    }
}

fn rewritten_control_event_bytes(event: &inference_providers::SSEEvent) -> Option<Bytes> {
    if event.is_done_marker() {
        return None;
    }

    let line = String::from_utf8_lossy(&event.raw_bytes);
    if line.trim_start().starts_with(':') {
        let mut bytes = event.raw_bytes.to_vec();
        if bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        } else {
            bytes.extend_from_slice(b"\n\n");
        }
        Some(Bytes::from(bytes))
    } else {
        None
    }
}

fn build_final_usage_chunk_bytes(
    usage: inference_providers::TokenUsage,
    template: &ChunkTemplate,
) -> Result<Option<Bytes>, serde_json::Error> {
    let Some((id, model, created, system_fingerprint)) = template else {
        return Ok(None);
    };

    let final_usage_chunk =
        inference_providers::StreamChunk::Chat(inference_providers::models::ChatCompletionChunk {
            id: id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: *created,
            model: model.clone(),
            system_fingerprint: system_fingerprint.clone(),
            choices: Vec::new(),
            usage: Some(usage),
            prompt_token_ids: None,
            modality: None,
            extra: std::collections::HashMap::new(),
        });

    serde_json::to_string(&final_usage_chunk)
        .map(|json_data| Some(Bytes::from(format!("data: {json_data}\n\n"))))
}

// Helper function to extract inference ID from a parsed stream chunk
fn extract_inference_id_from_chunk(chunk: &inference_providers::StreamChunk) -> Uuid {
    let id = match chunk {
        inference_providers::StreamChunk::Chat(c) => &c.id,
        inference_providers::StreamChunk::Text(c) => &c.id,
    };
    hash_inference_id_to_uuid(id)
}

// Convert MessageContent to serde_json::Value, preserving multimodal parts (images, audio, etc.)
fn message_content_to_value(content: &Option<MessageContent>) -> serde_json::Value {
    match content {
        None => serde_json::Value::String(String::new()),
        Some(content) => {
            let converted = crate::conversions::convert_content_to_vllm(content.clone());
            serde_json::to_value(&converted).expect("MessageContent should always be serializable")
        }
    }
}

// Convert HTTP ChatCompletionRequest to service CompletionRequest
fn convert_chat_request_to_service(
    request: &ChatCompletionRequest,
    user_id: Uuid,
    api_key_id: String,
    organization_id: Uuid,
    workspace_id: Uuid,
    body_hash: RequestBodyHash,
    request_id: Uuid,
) -> ServiceCompletionRequest {
    // `presence_penalty` / `frequency_penalty` are typed fields on
    // `ChatCompletionRequest`, so `#[serde(flatten)] extra` never captures them.
    // The service layer hardcodes the typed `ChatCompletionParams` penalty slots
    // to `None`, so forward them through `extra` (matching seed/logprobs and the
    // text-completion path) — otherwise they are silently dropped before reaching
    // the self-hosted backend (nearai/cloud-api #622).
    let mut extra = request.extra.clone();
    if let Some(presence_penalty) = request.presence_penalty {
        extra.insert(
            "presence_penalty".to_string(),
            serde_json::json!(presence_penalty),
        );
    }
    if let Some(frequency_penalty) = request.frequency_penalty {
        extra.insert(
            "frequency_penalty".to_string(),
            serde_json::json!(frequency_penalty),
        );
    }

    ServiceCompletionRequest {
        request_id,
        model: request.model.clone(),
        messages: request
            .messages
            .iter()
            .map(|msg| CompletionMessage {
                role: msg.role.clone(),
                content: message_content_to_value(&msg.content),
                tool_call_id: msg.tool_call_id.clone(),
                tool_calls: msg.tool_calls.as_ref().map(|calls| {
                    calls
                        .iter()
                        .map(|tc| services::completions::ports::CompletionToolCall {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.clone(),
                            thought_signature: tc.thought_signature.clone(),
                        })
                        .collect()
                }),
            })
            .collect(),
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        top_p: request.top_p,
        stop: request.stop.clone().map(|s| s.into_vec()),
        stream: request.stream,
        user_id: user_id.into(),
        n: request.n,
        api_key_id,
        organization_id,
        workspace_id,
        metadata: None,
        store: None,
        body_hash: body_hash.hash.clone(),
        response_id: None, // Direct chat completions API calls don't have a response_id
        skip_provider_chat_signature: false,
        extra,
    }
}

/// Decide whether to enable auto-redact based on the request, mutate the
/// service request to scrub the body field, and (if enabled) run the PII
/// detector + rewrite messages in place. Returns the placeholder map for
/// downstream un-redact; an empty map is also returned when auto-redact is
/// off.
/// The tuple is `(requested, map, classify_input_tokens)`. `classify_input_tokens`
/// is the number of tokens the privacy-filter billed for the classify pass,
/// for the caller to charge (nearai/cloud-api#602); it is 0 when auto-redact
/// is off or there was nothing to classify.
async fn maybe_redact(
    headers: &header::HeaderMap,
    service_request: &mut ServiceCompletionRequest,
    pool: &services::inference_provider_pool::InferenceProviderPool,
) -> Result<(bool, RedactionMap, i64), AutoRedactError> {
    let header_values: Vec<&str> = headers
        .get_all(auto_redact::AUTO_REDACT_HEADER)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    let body_field = service_request
        .extra
        .get(auto_redact::AUTO_REDACT_BODY_FIELD);
    let enabled = auto_redact::is_enabled(header_values.iter().copied(), body_field);

    // Always strip the body field so providers with strict JSON schemas
    // (e.g. Anthropic) don't 400 on unknown keys.
    auto_redact::strip_body_field(&mut service_request.extra);

    if !enabled {
        return Ok((false, RedactionMap::new(), 0));
    }

    let (map, classify_tokens) = auto_redact::redact_messages(
        &mut service_request.messages,
        auto_redact::DEFAULT_PII_MODEL,
        pool,
    )
    .await?;
    Ok((true, map, classify_tokens))
}

/// Convert an [`AutoRedactError`] into the user-facing error response.
/// Detector-unavailable is 503 (fail-closed per design); internal errors
/// are 500.
fn auto_redact_error_response(err: AutoRedactError) -> Response {
    match err {
        AutoRedactError::DetectorUnavailable(msg) => {
            tracing::error!(error = %msg, "auto_redact detector unavailable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                ResponseJson(ErrorResponse::new(
                    "PII redaction service is unavailable. Retry, or omit auto_redact to send the prompt as-is.".to_string(),
                    "auto_redact_unavailable".to_string(),
                )),
            )
                .into_response()
        }
        AutoRedactError::Internal(msg) => {
            tracing::error!(error = %msg, "auto_redact internal error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "PII redaction failed".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
                .into_response()
        }
    }
}

/// Upper bound on the spawned auto-redact billing task so a stuck billing DB
/// or model lookup can't leak a background task indefinitely.
const AUTO_REDACT_BILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Record usage for the privacy-filter classify pass that auto-redact runs
/// before a completion (nearai/cloud-api#602). The classify is a real
/// inference call to the PII model, so it is billed like an explicit
/// `/v1/privacy/classify`: `input_tokens × input_rate` on the privacy-filter
/// model, one record per request (a fresh v4 id, matching the explicit
/// privacy endpoints).
///
/// Best-effort: a model-lookup or record failure is logged and swallowed so
/// it never fails the user's completion (which bills separately via
/// `InterceptStream`). The classify already happened and cost GPU time, so
/// it is billed even if the downstream completion later fails.
async fn bill_auto_redact_classify(
    app_state: &AppState,
    api_key: &crate::middleware::auth::AuthenticatedApiKey,
    classify_tokens: i64,
) {
    if classify_tokens <= 0 {
        return; // nothing to bill
    }
    // Cap an anomalous count at the same `MAX_REASONABLE_TOKENS` the explicit
    // `/v1/privacy/{classify,redact}` paths use, emitting the same anomaly
    // metric — so x-auto-redact (which shares the privacy-filter classify
    // call) can't massively overcharge on a buggy/malicious provider response
    // instead of just clamping to i32::MAX. See nearai/cloud-api#602 review.
    const MAX_REASONABLE_TOKENS: i32 = 1_000_000;
    let input_tokens = if classify_tokens > MAX_REASONABLE_TOKENS as i64 {
        tracing::error!(
            token_count = classify_tokens,
            max_expected = MAX_REASONABLE_TOKENS,
            model = auto_redact::DEFAULT_PII_MODEL,
            "auto_redact: provider returned unreasonable classify token count - capping"
        );
        let model_tag = format!("model:{}", auto_redact::DEFAULT_PII_MODEL);
        let reason_tag = format!(
            "reason:{}",
            services::metrics::consts::REASON_TOKEN_OVERFLOW
        );
        app_state.metrics_service.record_count(
            services::metrics::consts::METRIC_PROVIDER_TOKEN_ANOMALIES,
            1,
            &[model_tag.as_str(), reason_tag.as_str()],
        );
        MAX_REASONABLE_TOKENS
    } else {
        classify_tokens as i32 // safe: 0 < classify_tokens <= 1_000_000
    };

    let model = match app_state
        .models_service
        .get_model_by_name(auto_redact::DEFAULT_PII_MODEL)
        .await
    {
        Ok(model) => model,
        Err(e) => {
            tracing::error!(
                error = %e,
                model = auto_redact::DEFAULT_PII_MODEL,
                "auto_redact: could not resolve PII model; classify usage not recorded"
            );
            return;
        }
    };

    let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "auto_redact: invalid API key id; classify usage not recorded");
            return;
        }
    };

    let usage_request = services::usage::RecordUsageServiceRequest {
        organization_id: api_key.organization.id.0,
        workspace_id: api_key.workspace.id.0,
        api_key_id,
        model_id: model.id,
        input_tokens,
        output_tokens: 0,
        cache_read_tokens: 0,
        inference_type: services::usage::ports::InferenceType::PrivacyClassify,
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(Uuid::new_v4()),
        provider_request_id: None,
        stop_reason: Some(services::usage::StopReason::Completed),
        response_id: None,
        image_count: None,
    };

    if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
        tracing::error!(error = %e, "auto_redact: failed to record classify usage");
    }
}

/// Walk a non-streaming chat response's text fields and substitute
/// placeholders with their originals. Mutates in place.
///
/// Covers content + reasoning fields and the JSON-encoded
/// `tool_calls[*].function.arguments` string. The latter is critical for
/// agentic flows where the model emits a tool call whose arguments echo
/// the user's PII — without un-redacting them, the placeholder leaks to
/// the client.
fn unredact_chat_response_in_place(
    response: &mut inference_providers::ChatCompletionResponse,
    map: &RedactionMap,
) {
    for choice in &mut response.choices {
        if let Some(content) = &mut choice.message.content {
            *content = map.unredact(content);
        }
        if let Some(reasoning) = &mut choice.message.reasoning_content {
            *reasoning = map.unredact(reasoning);
        }
        if let Some(reasoning) = &mut choice.message.reasoning {
            *reasoning = map.unredact(reasoning);
        }
        if let Some(refusal) = &mut choice.message.refusal {
            // A safety-tuned model may produce a refusal that quotes our
            // placeholders back ("I can't email <email1>"). Without
            // un-redacting, the placeholder leaks to the client.
            *refusal = map.unredact(refusal);
        }
        if let Some(tool_calls) = &mut choice.message.tool_calls {
            for tc in tool_calls {
                if let Some(args) = &mut tc.function.arguments {
                    // arguments is itself a JSON-encoded string. JSON-escape
                    // each replacement so PII containing `"`, `\`, or
                    // control chars doesn't corrupt the surrounding JSON.
                    *args = map.unredact_json_string(args);
                }
            }
        }
    }
}

/// Per-choice, per-field streaming un-redact state. For `n > 1`
/// completions the provider may interleave chunks for different choice
/// indices; each choice needs its own sliding tail buffer or split
/// placeholders get cross-contaminated. The text fields (`content`,
/// `reasoning_content`, `reasoning`) are kept independent for the same
/// reason — a model may emit them concurrently.
///
/// `tool_call_arguments` is keyed by `(choice_index, tool_call_index)`
/// because a single response can have multiple parallel tool calls, each
/// with its own arguments JSON stream.
#[derive(Default)]
struct StreamUnredactStates {
    content: std::collections::HashMap<i64, StreamUnredact>,
    reasoning_content: std::collections::HashMap<i64, StreamUnredact>,
    reasoning: std::collections::HashMap<i64, StreamUnredact>,
    tool_call_arguments: std::collections::HashMap<(i64, i64), StreamUnredact>,
}

fn unredact_field(
    states: &mut std::collections::HashMap<i64, StreamUnredact>,
    map: &Arc<RedactionMap>,
    idx: i64,
    text: &mut String,
    finalize: bool,
) {
    let s = states
        .entry(idx)
        .or_insert_with(|| StreamUnredact::new(map.clone()));
    // Finalize drains the tail (no further chunks coming for this
    // choice/field); regular path holds up to max_dummy_len bytes.
    *text = if finalize {
        s.drain(text)
    } else {
        s.process(text)
    };
}

/// Drain held tails for `idx` into the finish chunk's delta. Removes the
/// state entries from `states` so [`build_flush_chunks`] emits nothing
/// for this choice. Used only on chunks that carry `finish_reason` — see
/// [`unredact_chunk_in_place`] for the rationale.
///
/// For each text field: if the chunk already carries the field, the
/// previous inline `drain` call has already emptied that state's tail and
/// `flush` returns `""`, so we just remove the now-empty entry. If the
/// chunk did NOT carry the field, we remove the state and attach its
/// flushed tail to a freshly populated field on the chunk's delta.
fn finalize_choice_in_place(
    choice: &mut inference_providers::models::ChatChoice,
    idx: i64,
    states: &mut StreamUnredactStates,
) {
    fn merge_into(field: &mut Option<String>, tail: String) {
        if tail.is_empty() {
            return;
        }
        match field {
            // The inline drain path already produced the substituted
            // value, so the flushed tail is empty when we get here and
            // this branch shouldn't normally fire. Guard anyway.
            Some(existing) => existing.push_str(&tail),
            None => *field = Some(tail),
        }
    }

    let delta = choice
        .delta
        .get_or_insert(inference_providers::models::ChatDelta {
            role: None,
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            extra: Default::default(),
        });

    if let Some(s) = states.content.remove(&idx) {
        merge_into(&mut delta.content, s.flush());
    }
    if let Some(s) = states.reasoning_content.remove(&idx) {
        merge_into(&mut delta.reasoning_content, s.flush());
    }
    if let Some(s) = states.reasoning.remove(&idx) {
        merge_into(&mut delta.reasoning, s.flush());
    }

    // Tool-call arguments are keyed by `(choice_idx, tc_idx)`. Take every
    // entry for this choice; sort by tc_idx so the synthesized order is
    // stable; merge each non-empty tail into `delta.tool_calls`. If the
    // chunk already carries a matching tc_idx (inline-drained path), the
    // flushed tail is empty and the loop is a no-op for that entry.
    let mut tc_keys: Vec<(i64, i64)> = states
        .tool_call_arguments
        .keys()
        .filter(|(c, _)| *c == idx)
        .copied()
        .collect();
    tc_keys.sort_unstable();
    for key in tc_keys {
        let Some(s) = states.tool_call_arguments.remove(&key) else {
            continue;
        };
        let tail = s.flush();
        if tail.is_empty() {
            continue;
        }
        let (_, tc_idx) = key;
        let tool_calls = delta.tool_calls.get_or_insert_with(Vec::new);
        // Reuse the existing ToolCallDelta entry for this tc_idx if the
        // chunk already had one, so we don't emit two deltas for the
        // same logical tool call.
        let entry = tool_calls.iter_mut().find(|tc| tc.index == Some(tc_idx));
        match entry {
            Some(tc) => {
                let func =
                    tc.function
                        .get_or_insert(inference_providers::models::FunctionCallDelta {
                            name: None,
                            arguments: None,
                        });
                merge_into(&mut func.arguments, tail);
            }
            None => {
                tool_calls.push(inference_providers::models::ToolCallDelta {
                    id: None,
                    type_: None,
                    index: Some(tc_idx),
                    function: Some(inference_providers::models::FunctionCallDelta {
                        name: None,
                        arguments: Some(tail),
                    }),
                    thought_signature: None,
                });
            }
        }
    }
}

/// Apply streaming un-redact to a single parsed chunk, mutating any text
/// deltas (content + reasoning) in place. Stateful: per-choice tail
/// buffers carry across calls via `states`.
fn unredact_chunk_in_place(
    chunk: &mut inference_providers::StreamChunk,
    states: &mut StreamUnredactStates,
    map: &Arc<RedactionMap>,
) {
    match chunk {
        inference_providers::StreamChunk::Chat(c) => {
            for choice in &mut c.choices {
                let idx = choice.index;
                // A chunk carrying `finish_reason` is the last chunk
                // we'll see for this choice. Drain the held tail into
                // its fields rather than relying on the end-of-stream
                // flush (which emits a synthetic chunk AFTER the
                // finish_reason, missed by clients that stop reading
                // on finish_reason without waiting for `[DONE]`).
                let finalize = choice.finish_reason.is_some();
                if let Some(delta) = &mut choice.delta {
                    if let Some(content) = &mut delta.content {
                        unredact_field(&mut states.content, map, idx, content, finalize);
                    }
                    if let Some(rc) = &mut delta.reasoning_content {
                        unredact_field(&mut states.reasoning_content, map, idx, rc, finalize);
                    }
                    if let Some(r) = &mut delta.reasoning {
                        unredact_field(&mut states.reasoning, map, idx, r, finalize);
                    }
                    if let Some(tcs) = &mut delta.tool_calls {
                        for (pos, tc) in tcs.iter_mut().enumerate() {
                            // Per-tool-call streaming state keyed by
                            // (choice_index, tool_call_index). Use the
                            // delta's index if present; otherwise fall
                            // back to the *position* in this chunk so two
                            // parallel indexless tool calls don't collide.
                            let tc_idx = tc.index.unwrap_or(pos as i64);
                            if let Some(func) = &mut tc.function {
                                if let Some(args) = &mut func.arguments {
                                    // arguments is JSON-encoded; substitute
                                    // with JSON-escaped originals.
                                    let s = states
                                        .tool_call_arguments
                                        .entry((idx, tc_idx))
                                        .or_insert_with(|| {
                                            StreamUnredact::new_for_json_string(map.clone())
                                        });
                                    *args = if finalize {
                                        s.drain(args)
                                    } else {
                                        s.process(args)
                                    };
                                }
                            }
                        }
                    }
                }
                if finalize {
                    // Catch every per-choice state whose field was *absent*
                    // from this chunk's delta — those are the held tails
                    // the inline path can't see. Without this, a finish
                    // chunk with `delta: {}` leaves the tail in `states`
                    // and build_flush_chunks emits it AFTER the finish
                    // event (invisible to clients that stop on
                    // finish_reason). The inline-drained entries above
                    // are already empty; this loop just removes them.
                    finalize_choice_in_place(choice, idx, states);
                }
            }
        }
        inference_providers::StreamChunk::Text(c) => {
            for choice in &mut c.choices {
                let idx = choice.index;
                let finalize = choice.finish_reason.is_some();
                unredact_field(&mut states.content, map, idx, &mut choice.text, finalize);
            }
        }
    }
}

/// Snapshot of metadata from the first chunk we see in a stream, used to
/// build a synthetic flush chunk at end-of-stream with matching id/model.
/// Layout: `(id, model, created, system_fingerprint)`.
type ChunkTemplate = Option<(String, String, i64, Option<String>)>;

/// Drain a per-field state map at end-of-stream, emitting a synthetic
/// final SSE chunk for each choice index that still has bytes held in its
/// tail buffer. Without this, an upstream stream that ends mid-placeholder
/// would silently truncate the client's view of the response.
fn build_flush_chunks(states: &mut StreamUnredactStates, template: &ChunkTemplate) -> Vec<Bytes> {
    let Some((id, model, created, system_fingerprint)) = template else {
        return Vec::new();
    };

    let mut out: Vec<Bytes> = Vec::new();

    // --- Pass 1: content / reasoning_content / reasoning ---
    let mut pending: std::collections::BTreeMap<i64, (String, String, String)> =
        std::collections::BTreeMap::new();
    for (idx, st) in states.content.drain() {
        let text = st.flush();
        if !text.is_empty() {
            pending.entry(idx).or_default().0 = text;
        }
    }
    for (idx, st) in states.reasoning_content.drain() {
        let text = st.flush();
        if !text.is_empty() {
            pending.entry(idx).or_default().1 = text;
        }
    }
    for (idx, st) in states.reasoning.drain() {
        let text = st.flush();
        if !text.is_empty() {
            pending.entry(idx).or_default().2 = text;
        }
    }
    for (idx, (content, rc, r)) in pending {
        let chunk = inference_providers::models::ChatCompletionChunk {
            id: id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: *created,
            model: model.clone(),
            system_fingerprint: system_fingerprint.clone(),
            choices: vec![inference_providers::models::ChatChoice {
                index: idx,
                delta: Some(inference_providers::models::ChatDelta {
                    role: None,
                    content: if content.is_empty() {
                        None
                    } else {
                        Some(content)
                    },
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    reasoning_content: if rc.is_empty() { None } else { Some(rc) },
                    reasoning: if r.is_empty() { None } else { Some(r) },
                    extra: Default::default(),
                }),
                logprobs: None,
                finish_reason: None,
                token_ids: None,
            }],
            usage: None,
            prompt_token_ids: None,
            modality: None,
            extra: Default::default(),
        };
        if let Ok(s) = serde_json::to_string(&chunk) {
            out.push(Bytes::from(format!("data: {s}\n\n")));
        }
    }

    // --- Pass 2: tool_call_arguments ---
    // Each held tail becomes a synthetic tool-call delta with just the
    // arguments fragment. The placeholder may be incomplete (the LLM was
    // cut off mid-`<email1>`), in which case the literal bytes are emitted
    // — visible signal of truncation, but no silent loss of held bytes
    // and no leaking placeholder we never minted.
    let mut tc_drain: Vec<((i64, i64), String)> = states
        .tool_call_arguments
        .drain()
        .filter_map(|(key, st)| {
            let text = st.flush();
            if text.is_empty() {
                None
            } else {
                Some((key, text))
            }
        })
        .collect();
    // Stable ordering: choice index ascending, then tool_call index.
    tc_drain.sort_by_key(|((c, t), _)| (*c, *t));
    for ((choice_idx, tc_idx), args) in tc_drain {
        let chunk = inference_providers::models::ChatCompletionChunk {
            id: id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: *created,
            model: model.clone(),
            system_fingerprint: system_fingerprint.clone(),
            choices: vec![inference_providers::models::ChatChoice {
                index: choice_idx,
                delta: Some(inference_providers::models::ChatDelta {
                    role: None,
                    content: None,
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![inference_providers::models::ToolCallDelta {
                        id: None,
                        type_: None,
                        index: Some(tc_idx),
                        function: Some(inference_providers::models::FunctionCallDelta {
                            name: None,
                            arguments: Some(args),
                        }),
                        thought_signature: None,
                    }]),
                    reasoning_content: None,
                    reasoning: None,
                    extra: Default::default(),
                }),
                logprobs: None,
                finish_reason: None,
                token_ids: None,
            }],
            usage: None,
            prompt_token_ids: None,
            modality: None,
            extra: Default::default(),
        };
        if let Ok(s) = serde_json::to_string(&chunk) {
            out.push(Bytes::from(format!("data: {s}\n\n")));
        }
    }

    out
}

// Convert HTTP CompletionRequest to service CompletionRequest
#[allow(clippy::too_many_arguments)]
fn convert_text_request_to_service(
    request: &CompletionRequest,
    prompt: String,
    user_id: Uuid,
    api_key_id: String,
    organization_id: Uuid,
    workspace_id: Uuid,
    body_hash: RequestBodyHash,
    request_id: Uuid,
) -> ServiceCompletionRequest {
    // presence_penalty / frequency_penalty are standard sampling params the chat
    // backend accepts but the service request has no typed slot for, so forward
    // them through `extra` rather than dropping them. echo / logprobs / best_of
    // are rejected upstream (see unsupported_completion_param) — they have no
    // equivalent under the translate-to-chat path — so they never reach here set.
    let mut extra = request.extra.clone();
    if let Some(presence_penalty) = request.presence_penalty {
        extra.insert(
            "presence_penalty".to_string(),
            serde_json::json!(presence_penalty),
        );
    }
    if let Some(frequency_penalty) = request.frequency_penalty {
        extra.insert(
            "frequency_penalty".to_string(),
            serde_json::json!(frequency_penalty),
        );
    }

    ServiceCompletionRequest {
        request_id,
        model: request.model.clone(),
        messages: vec![CompletionMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(prompt),
            tool_call_id: None,
            tool_calls: None,
        }],
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        top_p: request.top_p,
        stop: request.stop.clone().map(|s| s.into_vec()),
        stream: request.stream,
        user_id: user_id.into(),
        n: request.n,
        api_key_id,
        organization_id,
        workspace_id,
        metadata: None,
        store: None,
        body_hash: body_hash.hash.clone(),
        response_id: None, // Direct text completions API calls don't have a response_id
        skip_provider_chat_signature: false,
        extra,
    }
}

/// Legacy text-completion parameters that have no equivalent under this
/// endpoint's translate-to-chat implementation. Returns the offending parameter
/// name when the request sets one to a non-default value so the caller can
/// reject with a 400 rather than silently returning OpenAI-incompatible
/// semantics. `presence_penalty` / `frequency_penalty` are intentionally absent
/// — they are forwarded to the provider (see convert_text_request_to_service).
fn unsupported_completion_param(request: &CompletionRequest) -> Option<&'static str> {
    if request.echo == Some(true) {
        // echo prepends the prompt to the completion; no chat equivalent.
        return Some("echo");
    }
    if request.logprobs.is_some() {
        // Legacy `logprobs` is an int (top-N tokens); the mapper always returns
        // null and chat logprobs are a different shape.
        return Some("logprobs");
    }
    if request.best_of.is_some_and(|b| b > 1) {
        // Server-side best-of-N selection is not plumbed through.
        return Some("best_of");
    }
    None
}

/// Create chat completion
///
/// Generate AI model responses for chat conversations. Supports both streaming and non-streaming modes.
/// OpenAI-compatible endpoint.
#[utoipa::path(
    post,
    path = "/v1/chat/completions",
    tag = "Chat",
    request_body = ChatCompletionRequest,
    responses(
        (status = 200, description = "Completion generated successfully", body = ChatCompletionResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 402, description = "Insufficient credits", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 529, description = "All inference backends overloaded; retry with exponential backoff", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn chat_completions(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    OpenAiJson(request): OpenAiJson<ChatCompletionRequest>,
) -> axum::response::Response {
    debug!(
        "Chat completions request from api key: {:?}",
        api_key.api_key.id
    );
    debug!(
        "Request model: {}, stream: {:?}, org: {}, workspace: {}",
        request.model, request.stream, api_key.organization.id, api_key.workspace.id.0
    );
    // Validate the request
    if let Err(error) = request.validate_request() {
        return (StatusCode::BAD_REQUEST, ResponseJson(error)).into_response();
    }

    // Generate a per-request correlation ID. Reuse the client's X-Request-Id if
    // present and parseable as a UUID; otherwise generate a fresh one. This ID
    // propagates downstream as X-Request-Id on every outbound inference call.
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::new_v4);

    // Create a span carrying correlation IDs so every log line within this
    // request carries request_id/org_id/workspace_id in Datadog.
    //
    // NOTE: we do NOT call span.enter() here. In async code, Span::enter()
    // stores the guard in a thread-local; when the future yields at an .await
    // the guard stays entered on that thread, so any other task scheduled on
    // the same thread incorrectly inherits this span as its parent, corrupting
    // log context. Use .instrument(span) on the inner async block instead.
    let span = tracing::info_span!(
        "chat_completions",
        request_id = %request_id,
        org_id = %api_key.organization.id.0,
        workspace_id = %api_key.workspace.id.0,
        model = %request.model,
    );

    chat_completions_inner(app_state, api_key, body_hash, headers, request, request_id)
        .instrument(span)
        .await
}

// Inner async fn so .instrument(span) wraps all awaits in the handler.
// Split from `chat_completions` only to correctly scope the tracing span:
// using span.enter() in an async fn risks the guard outliving an .await yield,
// causing other tasks on the same thread to inherit this span's context.
#[allow(clippy::too_many_arguments)]
async fn chat_completions_inner(
    app_state: crate::routes::api::AppState,
    api_key: crate::middleware::auth::AuthenticatedApiKey,
    body_hash: crate::middleware::RequestBodyHash,
    headers: header::HeaderMap,
    request: ChatCompletionRequest,
    request_id: Uuid,
) -> axum::response::Response {
    let request_hash = body_hash.hash.clone();

    // Convert HTTP request to service parameters
    // Note: Names are not passed - high-cardinality data is tracked via database, not metrics
    let mut service_request = convert_chat_request_to_service(
        &request,
        api_key.api_key.created_by_user_id.0,
        api_key.api_key.id.0.clone(),
        api_key.organization.id.0,
        api_key.workspace.id.0,
        body_hash,
        request_id,
    );

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    // Add validated headers to service_request.extra
    insert_encryption_headers(&encryption_headers, &mut service_request.extra);
    let e2ee_active = e2ee_requested(&encryption_headers);
    let include_stream_usage_in_response = chat_stream_include_usage_requested(&request);

    // Strict alias mode: refuse to serve through an alias before any
    // inference happens (issue #573).
    if let Err(resp) = reject_if_aliased(&app_state.models_service, &headers, &request.model).await
    {
        return resp;
    }

    // Pre-dispatch alias detection (issue #573): the canonical name when
    // the requested model is a registered alias, mirroring the resolution
    // the completion service is about to apply. Derived purely from the
    // catalog — never from the response's `model` echo, which carries no
    // signal about alias-ness: external providers may rewrite the upstream
    // model name (`provider_config.model_name`), and that override can
    // even equal the alias string itself. Cache-backed (no DB on the hot
    // path); advisory only — strict mode above stays authoritative.
    let alias_canonical = app_state
        .models_service
        .resolve_alias_cached(&request.model)
        .await;
    let resolved_model_name = alias_canonical.as_deref().unwrap_or(&request.model);
    let model_attestation_supported = if request.stream == Some(true) {
        match app_state.models_service.get_models_with_pricing().await {
            Ok(models) => models
                .iter()
                .find(|model| model.model_name == resolved_model_name)
                .map(|model| model.attestation_supported),
            Err(error) => {
                tracing::warn!(
                    model = %request.model,
                    error = %error,
                    "Failed to read cached model metadata for stream usage shaping; preserving raw passthrough"
                );
                None
            }
        }
    } else {
        None
    };
    let usage_mode = chat_stream_usage_mode(&request, model_attestation_supported, e2ee_active);
    let rewrite_public_stream_usage = usage_mode.rewrite_public_stream_usage;
    let gateway_signature_enabled = usage_mode.gateway_signature_enabled;
    service_request.skip_provider_chat_signature = gateway_signature_enabled;

    // Auto-redact (opt-in via x-auto-redact header or auto_redact body field).
    // On success this may rewrite service_request.messages to substitute
    // placeholders for PII; the returned map drives the response un-redact.
    let (auto_redact_requested, redaction_map, auto_redact_classify_tokens) = match maybe_redact(
        &headers,
        &mut service_request,
        &app_state.inference_provider_pool,
    )
    .await
    {
        Ok(out) => out,
        Err(e) => return auto_redact_error_response(e),
    };
    // Bill the privacy-filter classify pass that auto-redact ran (it is a
    // real inference call to the PII model). Charged like an explicit
    // `/v1/privacy/classify`. Spawned (not awaited) and time-bounded so a
    // slow/exhausted billing DB or model lookup can never stall the user's
    // completion before dispatch — the classify already succeeded, and the
    // chat bills separately via InterceptStream. See nearai/cloud-api#602.
    if auto_redact_requested {
        let app_state = app_state.clone();
        let api_key = api_key.clone();
        tokio::spawn(async move {
            if tokio::time::timeout(
                AUTO_REDACT_BILL_TIMEOUT,
                bill_auto_redact_classify(&app_state, &api_key, auto_redact_classify_tokens),
            )
            .await
            .is_err()
            {
                tracing::error!("auto_redact: classify billing timed out; usage not recorded");
            }
        });
    }
    // Treat auto-redact as effectively *enabled* only when the detector
    // actually minted placeholders. A request that opts in but contains no
    // PII has nothing to substitute, so we skip the response re-serialize
    // (preserves raw-bytes signing) and the streaming wrap (preserves the
    // existing debug log path).
    let auto_redact_enabled = auto_redact_requested && !redaction_map.is_empty();
    let redaction_map = Arc::new(redaction_map);

    // Check if streaming is requested
    if request.stream == Some(true) {
        // Call the streaming completion service
        match app_state
            .completion_service
            .create_chat_completion_stream(service_request)
            .await
        {
            Ok(stream) => {
                // Make stream peekable to extract chat_id for Inference-Id header
                let mut peekable_stream = Box::pin(stream.peekable());

                // Peek for the Inference-Id header. Control events (e.g. an
                // upstream keepalive comment) may precede the first data
                // chunk; consume and stash them so they are still forwarded
                // to the client in order (they're part of the signed byte
                // stream — issue #701). Bounded by MAX_LEADING_CONTROL_EVENTS
                // so a misbehaving upstream that only emits keepalives can't
                // stall response start or grow this buffer unbounded — past
                // the cap we proceed without an Inference-Id and let the rest
                // flow through the byte stream below.
                let mut leading_control: Vec<
                    Result<inference_providers::SSEEvent, inference_providers::CompletionError>,
                > = Vec::new();
                let inference_id = loop {
                    let is_control = match peekable_stream.as_mut().peek().await {
                        Some(Ok(event)) => {
                            if let Some(chunk) = &event.chunk {
                                break Some(extract_inference_id_from_chunk(chunk));
                            }
                            true
                        }
                        _ => break None,
                    };
                    if is_control {
                        if leading_control.len() >= MAX_LEADING_CONTROL_EVENTS {
                            break None;
                        }
                        if let Some(ev) = peekable_stream.next().await {
                            leading_control.push(ev);
                        }
                    }
                };

                // Warning to inject into the first streamed chunk. Skipped
                // for E2EE (the chunks are opaque; the response header is
                // the only signal there).
                let alias_warning_pending: Arc<std::sync::Mutex<Option<String>>> =
                    Arc::new(std::sync::Mutex::new(
                        alias_canonical
                            .as_ref()
                            .filter(|_| !e2ee_active)
                            .map(|canonical| alias_warning_message(&request.model, canonical)),
                    ));
                // Alias-served responses can't use byte-exact passthrough: the
                // first chunk is rewritten to carry the warning, and the TEE
                // signs under the canonical model name anyway.
                let alias_served = alias_canonical.is_some();

                if inference_id.is_none() {
                    tracing::warn!(
                        organization_id = %api_key.organization.id.0,
                        model = %request.model,
                        "Could not extract inference ID from first chunk for chat completion (streaming)"
                    );
                }

                let stream_error_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

                let error_count_clone = stream_error_count.clone();
                let request_model = request.model.clone();
                let organization_id = api_key.organization.id.0;

                // Set when the upstream's own `data: [DONE]` terminator was
                // forwarded verbatim, so the end-of-stream tail doesn't
                // append a second, gateway-minted one.
                let upstream_done_forwarded = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let upstream_done_for_chain = upstream_done_forwarded.clone();

                // Per-stream un-redact state, keyed by choice index so n>1
                // completions don't cross-contaminate sliding tails. When
                // auto-redact is off, the map stays empty and we skip the
                // mutex hop entirely.
                let unredact_states: Arc<tokio::sync::Mutex<StreamUnredactStates>> =
                    Arc::new(tokio::sync::Mutex::new(StreamUnredactStates::default()));
                // Capture the first chunk's metadata so the end-of-stream
                // flush can synthesize a final SSE chunk with matching
                // id/model/created.
                let chunk_template: Arc<tokio::sync::Mutex<ChunkTemplate>> =
                    Arc::new(tokio::sync::Mutex::new(None));
                let chunk_template_for_chain = chunk_template.clone();
                let unredact_states_for_chain = unredact_states.clone();
                let redaction_map_for_chunks = redaction_map.clone();
                let final_stream_usage = Arc::new(tokio::sync::Mutex::new(
                    None::<inference_providers::TokenUsage>,
                ));
                let final_stream_usage_for_chain = final_stream_usage.clone();
                let public_signature_hasher = Arc::new(tokio::sync::Mutex::new(Sha256::new()));
                let public_signature_chat_id = Arc::new(tokio::sync::Mutex::new(None::<String>));
                let public_signature_hasher_for_chain = public_signature_hasher.clone();
                let public_signature_chat_id_for_chain = public_signature_chat_id.clone();
                let attestation_service_for_chain = app_state.attestation_service.clone();

                // Re-attach any stashed leading control events, then convert
                // to a raw bytes stream.
                let event_stream = futures::stream::iter(leading_control).chain(peekable_stream);

                let byte_stream = event_stream
                    .filter_map(move |result| {
                        let error_count_inner = error_count_clone.clone();
                        let model_for_err = request_model.clone();
                        let states = unredact_states.clone();
                        let template = chunk_template.clone();
                        let map = redaction_map_for_chunks.clone();
                        let pending_warning = alias_warning_pending.clone();
                        let upstream_done = upstream_done_forwarded.clone();
                        let include_stream_usage_in_response = include_stream_usage_in_response;
                        let rewrite_public_stream_usage = rewrite_public_stream_usage;
                        let gateway_signature_enabled = gateway_signature_enabled;
                        let public_signature_hasher = public_signature_hasher.clone();
                        let public_signature_chat_id = public_signature_chat_id.clone();
                        let final_stream_usage = final_stream_usage.clone();
                        async move {
                            match result {
                                Ok(event) => {
                                    // Byte-exact passthrough (issue #701): when no public
                                    // chunk rewriting is active, forward the upstream wire
                                    // bytes untouched. Explicit include_usage shaping needs
                                    // parsed-chunk serialization; encrypted, default, and
                                    // continuous-usage streams preserve passthrough.
                                    //
                                    // Disabled for alias-served responses: those inject
                                    // a top-level `warning` into the first chunk (below),
                                    // and are non-byte-verifiable anyway since the TEE
                                    // signs under the canonical model name, not the alias
                                    // the client requested.
                                    if event.raw_passthrough
                                        && !auto_redact_enabled
                                        && !alias_served
                                        && !rewrite_public_stream_usage
                                    {
                                        if event.is_done_marker() {
                                            upstream_done
                                                .store(true, std::sync::atomic::Ordering::Relaxed);
                                        }
                                        return Some(Ok::<Bytes, Infallible>(event.raw_bytes));
                                    }

                                    // Re-serialization path: auto-redact rewrites chunk
                                    // text, and non-OpenAI upstreams (Gemini native,
                                    // Anthropic) need normalization to OpenAI format.
                                    // Control lines carry no parsed payload; forward
                                    // them raw so keepalives/comments are preserved.
                                    // Hold [DONE] back for rewritten streams so the
                                    // tail can emit final usage, append [DONE], and
                                    // store any gateway signature before completion.
                                    let Some(mut chunk) = event.chunk else {
                                        if event.is_done_marker() {
                                            if auto_redact_enabled || rewrite_public_stream_usage {
                                                return None;
                                            }
                                            upstream_done.store(
                                                true,
                                                std::sync::atomic::Ordering::Relaxed,
                                            );
                                        }
                                        let control_bytes = if rewrite_public_stream_usage {
                                            rewritten_control_event_bytes(&event)
                                        } else {
                                            Some(event.raw_bytes)
                                        };
                                        if let Some(control_bytes) = control_bytes {
                                            if gateway_signature_enabled {
                                                public_signature_hasher
                                                    .lock()
                                                    .await
                                                    .update(&control_bytes);
                                            }
                                            return Some(Ok::<Bytes, Infallible>(control_bytes));
                                        }
                                        return None;
                                    };
                                    if let inference_providers::StreamChunk::Chat(chat) = &chunk {
                                        {
                                            let mut t = template.lock().await;
                                            if t.is_none() {
                                                *t = Some((
                                                    chat.id.clone(),
                                                    chat.model.clone(),
                                                    chat.created,
                                                    chat.system_fingerprint.clone(),
                                                ));
                                            }
                                        }
                                        if gateway_signature_enabled {
                                            let mut chat_id = public_signature_chat_id.lock().await;
                                            if chat_id.is_none() {
                                                *chat_id = Some(chat.id.clone());
                                            }
                                        }
                                    }
                                    if rewrite_public_stream_usage {
                                        let mut final_usage = final_stream_usage.lock().await;
                                        if !prepare_stream_chunk_for_client(
                                            &mut chunk,
                                            include_stream_usage_in_response,
                                            &mut final_usage,
                                        ) {
                                            return None;
                                        }
                                    }

                                    if auto_redact_enabled {
                                        // Swap minted placeholders in this
                                        // chunk's text deltas back to originals.
                                        let mut s = states.lock().await;
                                        unredact_chunk_in_place(&mut chunk, &mut s, &map);
                                    }

                                    // Serialize the parsed chunk (normalized to OpenAI format)
                                    // instead of forwarding raw provider bytes, which may be
                                    // in a provider-specific format (e.g. Gemini native).
                                    // The first chunk of an alias-served response gets a
                                    // top-level "warning" so the substitution isn't silent.
                                    let alias_warning =
                                        pending_warning.lock().ok().and_then(|mut g| g.take());
                                    let json_data = match alias_warning {
                                        Some(warning) => {
                                            serde_json::to_value(&chunk).map(|mut v| {
                                                if let Some(obj) = v.as_object_mut() {
                                                    obj.insert(
                                                        "warning".to_string(),
                                                        serde_json::Value::String(warning),
                                                    );
                                                }
                                                v.to_string()
                                            })
                                        }
                                        None => serde_json::to_string(&chunk),
                                    }
                                    .unwrap_or_else(|e| {
                                        tracing::error!(
                                            %organization_id,
                                            "Failed to serialize stream chunk: {e}"
                                        );
                                        "{}".to_string()
                                    });
                                    // Suppress per-chunk debug logging when
                                    // auto_redact is enabled: the chunk now
                                    // holds the user's original PII (we just
                                    // un-redacted it). Logging it would
                                    // defeat the privacy guarantee at debug.
                                    if !auto_redact_enabled {
                                        tracing::debug!("Completion stream event: {}", json_data);
                                    }
                                    // Format as SSE event with proper newlines
                                    let sse_bytes = Bytes::from(format!("data: {json_data}\n\n"));
                                    if gateway_signature_enabled {
                                        public_signature_hasher
                                            .lock()
                                            .await
                                            .update(&sse_bytes);
                                    }
                                    Some(Ok::<Bytes, Infallible>(sse_bytes))
                                }
                                Err(e) => {
                                    let count = error_count_inner
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if count == 0 {
                                        tracing::error!(
                                            %organization_id,
                                            model = %model_for_err,
                                            error_type = %completion_stream_error_category(&e),
                                            "Completion stream error"
                                        );
                                    }
                                    Some(Ok::<Bytes, Infallible>(sse_error_frame(&e)))
                                }
                            }
                        }
                    })
                    .chain(
                        futures::stream::once({
                            // End-of-stream tail: emit any flush chunks (held
                            // tail bytes that the sliding window didn't get to
                            // resolve) inline with [DONE]. They're framed as
                            // separate SSE events by `\n\n` so the client sees
                            // them as distinct deltas. When the upstream's own
                            // [DONE] was already forwarded verbatim
                            // (passthrough), nothing is appended — the byte
                            // stream must end exactly as the upstream's did.
                            let organization_id = api_key.organization.id.0;
                            let model_name = request.model.clone();
                            let request_hash = request_hash.clone();
                            async move {
                                let mut combined: Vec<u8> = Vec::new();
                                let error_count_final =
                                    stream_error_count.load(std::sync::atomic::Ordering::Relaxed);
                                if auto_redact_enabled {
                                    let mut states = unredact_states_for_chain.lock().await;
                                    let template = chunk_template_for_chain.lock().await.clone();
                                    for bytes in build_flush_chunks(&mut states, &template) {
                                        combined.extend_from_slice(&bytes);
                                    }
                                }

                                if rewrite_public_stream_usage && include_stream_usage_in_response {
                                    let final_usage = final_stream_usage_for_chain.lock().await.clone();
                                    let template = chunk_template_for_chain.lock().await.clone();
                                    if error_count_final > 0 {
                                        if final_usage.is_some() {
                                            tracing::warn!(
                                                %organization_id,
                                                model = %model_name,
                                                total_stream_errors = error_count_final,
                                                "Suppressing final usage chunk because the stream ended with errors"
                                            );
                                        }
                                    } else {
                                        match final_usage {
                                            Some(usage) => match build_final_usage_chunk_bytes(usage, &template) {
                                                Ok(Some(bytes)) => {
                                                    combined.extend_from_slice(&bytes);
                                                }
                                                Ok(None) => {
                                                    tracing::warn!(
                                                        %organization_id,
                                                        model = %model_name,
                                                        "Cannot emit final usage chunk: no chat chunk template observed"
                                                    );
                                                }
                                                Err(e) => {
                                                    tracing::error!(
                                                        %organization_id,
                                                        model = %model_name,
                                                        "Failed to serialize final usage chunk: {e}"
                                                    );
                                                }
                                            },
                                            None => {
                                                tracing::warn!(
                                                    %organization_id,
                                                    model = %model_name,
                                                    "include_usage was requested but upstream did not provide usage; omitting final usage chunk"
                                                );
                                            }
                                        }
                                    }
                                }

                                if error_count_final > 1 {
                                    tracing::error!(
                                        %organization_id,
                                        model = %model_name,
                                        total_stream_errors = error_count_final,
                                        "Completion stream ended with multiple errors"
                                    );
                                }

                                if !upstream_done_for_chain
                                    .load(std::sync::atomic::Ordering::Relaxed)
                                {
                                    combined.extend_from_slice(b"data: [DONE]\n\n");
                                }

                                if gateway_signature_enabled && error_count_final == 0 {
                                    let response_hash = {
                                        let mut hasher =
                                            public_signature_hasher_for_chain.lock().await;
                                        hasher.update(&combined);
                                        hex::encode(hasher.clone().finalize())
                                    };

                                    if let Some(chat_id) =
                                        public_signature_chat_id_for_chain.lock().await.clone()
                                    {
                                        match tokio::time::timeout(
                                            Duration::from_secs(
                                                STREAM_SIGNATURE_STORE_TIMEOUT_SECS,
                                            ),
                                            attestation_service_for_chain.store_chat_signature(
                                                &chat_id,
                                                request_hash,
                                                response_hash,
                                            ),
                                        )
                                        .await
                                        {
                                            Ok(Ok(())) => {}
                                            Ok(Err(e)) => {
                                                tracing::error!(
                                                    %organization_id,
                                                    model = %model_name,
                                                    error = %e,
                                                    "Failed to store public stream chat signature"
                                                );
                                            }
                                            Err(_) => {
                                                tracing::error!(
                                                    %organization_id,
                                                    model = %model_name,
                                                    "Timeout storing public stream chat signature"
                                                );
                                            }
                                        }
                                    } else {
                                        tracing::warn!(
                                            %organization_id,
                                            model = %model_name,
                                            "Cannot store public stream chat signature: no chat_id observed"
                                        );
                                    }
                                }

                                if combined.is_empty() {
                                    // Avoid emitting an empty body frame.
                                    None
                                } else {
                                    Some(Ok::<Bytes, Infallible>(Bytes::from(combined)))
                                }
                            }
                        })
                        .filter_map(std::future::ready),
                    );

                // Return raw streaming response with SSE headers
                let mut response_builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive");

                // Collect CORS-exposed header names so the
                // Access-Control-Expose-Headers value is a single
                // comma-joined list (repeated header lines are not
                // consistently merged by browsers).
                let mut exposed_headers: Vec<&str> = Vec::new();

                // Add Inference-Id header if available
                if let Some(uuid) = inference_id {
                    response_builder =
                        response_builder.header(HEADER_INFERENCE_ID, uuid.to_string());
                    exposed_headers.push(HEADER_INFERENCE_ID);
                }

                // Announce alias substitution so it is never silent (issue #573).
                // Guarded HeaderValue construction: a header-invalid byte in a
                // model name must not panic the `.body().unwrap()` below.
                if let Some(canonical) = &alias_canonical {
                    if let Ok(value) = header::HeaderValue::from_str(&format!(
                        "{} -> {}",
                        request.model, canonical
                    )) {
                        response_builder =
                            response_builder.header(HEADER_MODEL_ALIAS_RESOLVED, value);
                        exposed_headers.push(HEADER_MODEL_ALIAS_RESOLVED);
                    }
                }

                if !exposed_headers.is_empty() {
                    response_builder = response_builder
                        .header("Access-Control-Expose-Headers", exposed_headers.join(", "));
                }

                response_builder
                    .body(Body::from_stream(byte_stream))
                    .unwrap()
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (
                    status_code,
                    ResponseJson::<ErrorResponse>(domain_error.into()),
                )
                    .into_response()
            }
        }
    } else {
        // Call the non-streaming completion service
        match app_state
            .completion_service
            .create_chat_completion(service_request)
            .await
        {
            Ok(mut response_with_bytes) => {
                // Extract inference ID from response ID (reuse same hashing as usage tracking)
                let inference_id =
                    Some(hash_inference_id_to_uuid(&response_with_bytes.response.id));

                // When auto-redact is enabled, we substitute placeholders back to
                // originals and re-serialize. The provider's raw_bytes are over the
                // redacted form; we deliberately drop that signed payload because
                // the client opted into munging the response.
                let body_bytes = if auto_redact_enabled {
                    unredact_chat_response_in_place(
                        &mut response_with_bytes.response,
                        &redaction_map,
                    );
                    match serde_json::to_vec(&response_with_bytes.response) {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::error!(error = %e, "failed to re-serialize unredacted chat response");
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                ResponseJson(ErrorResponse::new(
                                    "Failed to assemble response".to_string(),
                                    "internal_server_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    }
                } else {
                    // Return the exact bytes from the provider for hash verification.
                    // This ensures clients can hash the response and compare with
                    // attestation endpoints.
                    response_with_bytes.raw_bytes
                };

                // Annotate alias-served responses with a top-level "warning"
                // (issue #573). This re-serializes the body, so — like
                // auto-redact — it deliberately gives up raw-bytes hash
                // verification for these responses; clients that need the
                // raw-bytes guarantee should send the canonical model name
                // (or x-no-aliasing). E2EE bodies are opaque and are left
                // untouched (inject_warning_field returns None for them, and
                // we don't attempt it) — the header below is the signal.
                let body_bytes = match &alias_canonical {
                    Some(canonical) if !e2ee_active => inject_warning_field(
                        &body_bytes,
                        &alias_warning_message(&request.model, canonical),
                    )
                    .unwrap_or(body_bytes),
                    _ => body_bytes,
                };

                let mut response_builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json");

                // Collect CORS-exposed header names so the
                // Access-Control-Expose-Headers value is a single
                // comma-joined list (repeated header lines are not
                // consistently merged by browsers).
                let mut exposed_headers: Vec<&str> = Vec::new();

                // Add Inference-Id header if available
                if let Some(uuid) = inference_id {
                    response_builder =
                        response_builder.header(HEADER_INFERENCE_ID, uuid.to_string());
                    exposed_headers.push(HEADER_INFERENCE_ID);
                }

                // Announce alias substitution so it is never silent (issue #573).
                // Guarded HeaderValue construction: a header-invalid byte in a
                // model name must not panic the `.body().unwrap()` below.
                if let Some(canonical) = &alias_canonical {
                    if let Ok(value) = header::HeaderValue::from_str(&format!(
                        "{} -> {}",
                        request.model, canonical
                    )) {
                        response_builder =
                            response_builder.header(HEADER_MODEL_ALIAS_RESOLVED, value);
                        exposed_headers.push(HEADER_MODEL_ALIAS_RESOLVED);
                    }
                }

                if !exposed_headers.is_empty() {
                    response_builder = response_builder
                        .header("Access-Control-Expose-Headers", exposed_headers.join(", "));
                }

                response_builder.body(Body::from(body_bytes)).unwrap()
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (
                    status_code,
                    ResponseJson::<ErrorResponse>(domain_error.into()),
                )
                    .into_response()
            }
        }
    }
}

/// Create text completion
///
/// Generate AI model responses for text prompts. OpenAI-compatible endpoint.
#[utoipa::path(
    post,
    path = "/v1/completions",
    tag = "Chat",
    request_body = CompletionRequest,
    responses(
        (status = 200, description = "Completion generated successfully", body = CompletionResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 529, description = "All inference backends overloaded; retry with exponential backoff", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn completions(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    OpenAiJson(request): OpenAiJson<CompletionRequest>,
) -> axum::response::Response {
    debug!(
        "Text completions request from api key: {:?}",
        api_key.api_key.id
    );
    debug!(
        "Request model: {}, stream: {:?}, org: {}, workspace: {}",
        request.model, request.stream, api_key.organization.id, api_key.workspace.id.0
    );

    // Validate the request
    if let Err(error) = request.validate_request() {
        return (StatusCode::BAD_REQUEST, ResponseJson(error)).into_response();
    }

    // Per-request correlation ID: reuse the client's X-Request-Id if present and
    // parseable as a UUID, otherwise generate one. Matches chat_completions.
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::new_v4);

    // See chat_completions: do NOT span.enter() in async code; .instrument() the
    // inner future so the span wraps every await without leaking across tasks.
    let span = tracing::info_span!(
        "completions",
        request_id = %request_id,
        org_id = %api_key.organization.id.0,
        workspace_id = %api_key.workspace.id.0,
        model = %request.model,
    );

    completions_inner(app_state, api_key, body_hash, headers, request, request_id)
        .instrument(span)
        .await
}

// The legacy text-completions endpoint is implemented by translating the
// `prompt` into a single user chat message (see convert_text_request_to_service)
// and reshaping the chat response back into the `object: "text_completion"`
// format. Consequences worth knowing:
//   - the backend applies its chat template to the prompt, so output is not
//     identical to a raw completion against the base model;
//   - the response is synthesized, so unlike /v1/chat/completions it is not
//     byte-verifiable against attestation (the Inference-Id header still maps
//     to the provider response id).
// Usage tracking/billing is handled inside the completion service, same as chat.
//
// E2E encryption and auto-redact are NOT supported here: both rely on the chat
// path forwarding the provider's bytes (encrypted / un-redacted) straight to the
// client, which is incompatible with reshaping the response into text_completion
// format. Rather than silently bypass these privacy features (sending plaintext /
// un-redacted prompts), we reject such requests with a 400.
#[allow(clippy::too_many_arguments)]
async fn completions_inner(
    app_state: AppState,
    api_key: AuthenticatedApiKey,
    body_hash: RequestBodyHash,
    headers: header::HeaderMap,
    request: CompletionRequest,
    request_id: Uuid,
) -> axum::response::Response {
    // Reject E2E encryption: validate for parity (an invalid version still 400s
    // the same way chat does), then refuse if any encryption header is present.
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(h) => h,
        Err(err) => return err.into_response(),
    };
    if encryption_headers.signing_algo.is_some()
        || encryption_headers.client_pub_key.is_some()
        || encryption_headers.model_pub_key.is_some()
        || encryption_headers.encryption_version.is_some()
        || encryption_headers.encrypt_all_fields.is_some()
    {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "End-to-end encryption is not supported on /v1/completions; use /v1/chat/completions".to_string(),
                "unsupported_parameter".to_string(),
            )),
        )
            .into_response();
    }

    // Reject auto-redact opt-in (header or body field) rather than sending the
    // raw prompt to the provider with redaction silently disabled.
    let auto_redact_headers: Vec<&str> = headers
        .get_all(auto_redact::AUTO_REDACT_HEADER)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    if auto_redact::is_enabled(
        auto_redact_headers.iter().copied(),
        request.extra.get(auto_redact::AUTO_REDACT_BODY_FIELD),
    ) {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Auto-redact is not supported on /v1/completions; use /v1/chat/completions"
                    .to_string(),
                "unsupported_parameter".to_string(),
            )),
        )
            .into_response();
    }

    // Reject advertised-but-unmappable legacy params instead of silently
    // dropping them and returning OpenAI-incompatible semantics.
    if let Some(param) = unsupported_completion_param(&request) {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                format!(
                    "Parameter '{param}' is not supported on /v1/completions; use /v1/chat/completions"
                ),
                "unsupported_parameter".to_string(),
            )),
        )
            .into_response();
    }

    // Resolve the prompt to the single string this endpoint supports. Batch
    // (array) and token-id prompts deserialize fine but have no mapping under
    // the translate-to-chat path, so reject them with a clean 400.
    let prompt = match request.prompt.single_text() {
        Ok(p) => p.to_string(),
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    msg.to_string(),
                    "unsupported_parameter".to_string(),
                )),
            )
                .into_response();
        }
    };

    // Strict alias mode + pre-dispatch alias detection (issue #573) — the
    // service resolves aliases for this endpoint exactly like chat, so it
    // gets the same contract: x-no-aliasing rejects, and aliased responses
    // carry the warning + x-model-alias-resolved header.
    if let Err(resp) = reject_if_aliased(&app_state.models_service, &headers, &request.model).await
    {
        return resp;
    }
    let alias_canonical = app_state
        .models_service
        .resolve_alias_cached(&request.model)
        .await;

    let service_request = convert_text_request_to_service(
        &request,
        prompt,
        api_key.api_key.created_by_user_id.0,
        api_key.api_key.id.0.clone(),
        api_key.organization.id.0,
        api_key.workspace.id.0,
        body_hash,
        request_id,
    );

    if request.stream == Some(true) {
        match app_state
            .completion_service
            .create_chat_completion_stream(service_request)
            .await
        {
            Ok(stream) => {
                // Peek the first data chunk to surface the Inference-Id
                // header, consuming any leading control events. This route
                // reshapes chat chunks into text-completion format, so
                // control lines are never forwarded (no byte passthrough
                // here by design) and the consumed events can be discarded.
                // Bounded so a keepalive-only upstream can't stall the
                // response: past the cap we proceed without an Inference-Id.
                let mut peekable_stream = Box::pin(stream.peekable());
                let mut control_skipped = 0usize;
                let inference_id = loop {
                    let is_control = match peekable_stream.as_mut().peek().await {
                        Some(Ok(event)) => {
                            if let Some(chunk) = &event.chunk {
                                break Some(extract_inference_id_from_chunk(chunk));
                            }
                            true
                        }
                        _ => break None,
                    };
                    if is_control {
                        if control_skipped >= MAX_LEADING_CONTROL_EVENTS {
                            break None;
                        }
                        control_skipped += 1;
                        peekable_stream.next().await;
                    }
                };

                if inference_id.is_none() {
                    tracing::warn!(
                        organization_id = %api_key.organization.id.0,
                        model = %request.model,
                        "Could not extract inference ID from first chunk for text completion (streaming)"
                    );
                }

                let organization_id = api_key.organization.id.0;
                let model_for_err = request.model.clone();

                // Warning to inject into the first streamed chunk of an
                // alias-served response (issue #573).
                let alias_warning_pending: Arc<std::sync::Mutex<Option<String>>> =
                    Arc::new(std::sync::Mutex::new(alias_canonical.as_ref().map(
                        |canonical| alias_warning_message(&request.model, canonical),
                    )));
                let pending_warning = alias_warning_pending.clone();

                let byte_stream = peekable_stream
                    .filter_map(move |result| {
                        let model_for_err = model_for_err.clone();
                        let pending_warning = pending_warning.clone();
                        std::future::ready(match result {
                            // Control lines (blank/comment/[DONE]) carry no
                            // parsed payload — skip; the gateway appends its
                            // own [DONE] terminator below. This route reshapes
                            // chat chunks into text-completion format, so it
                            // always re-serializes (no byte passthrough).
                            Ok(event) => event.chunk.map(|chunk| {
                                let text_chunk = chat_chunk_to_text_chunk(chunk);
                                // The first chunk of an alias-served response
                                // gets a top-level "warning" (issue #573).
                                let alias_warning =
                                    pending_warning.lock().ok().and_then(|mut g| g.take());
                                let json_data = match alias_warning {
                                    Some(warning) => {
                                        serde_json::to_value(&text_chunk).map(|mut v| {
                                            if let Some(obj) = v.as_object_mut() {
                                                obj.insert(
                                                    "warning".to_string(),
                                                    serde_json::Value::String(warning),
                                                );
                                            }
                                            v.to_string()
                                        })
                                    }
                                    None => serde_json::to_string(&text_chunk),
                                }
                                .unwrap_or_else(|e| {
                                    tracing::error!(
                                        %organization_id,
                                        "Failed to serialize text completion chunk: {e}"
                                    );
                                    "{}".to_string()
                                });
                                Ok::<Bytes, Infallible>(Bytes::from(format!(
                                    "data: {json_data}\n\n"
                                )))
                            }),
                            Err(e) => {
                                tracing::error!(
                                    %organization_id,
                                    model = %model_for_err,
                                    error_type = %completion_stream_error_category(&e),
                                    "Text completion stream error"
                                );
                                Some(Ok::<Bytes, Infallible>(sse_error_frame(&e)))
                            }
                        })
                    })
                    .chain(futures::stream::once(async move {
                        Ok::<Bytes, Infallible>(Bytes::from_static(b"data: [DONE]\n\n"))
                    }));

                let mut response_builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive");

                let mut exposed_headers: Vec<&str> = Vec::new();
                if let Some(uuid) = inference_id {
                    response_builder =
                        response_builder.header(HEADER_INFERENCE_ID, uuid.to_string());
                    exposed_headers.push(HEADER_INFERENCE_ID);
                }
                // Announce alias substitution so it is never silent (issue #573)
                if let Some(canonical) = &alias_canonical {
                    if let Ok(value) = header::HeaderValue::from_str(&format!(
                        "{} -> {}",
                        request.model, canonical
                    )) {
                        response_builder =
                            response_builder.header(HEADER_MODEL_ALIAS_RESOLVED, value);
                        exposed_headers.push(HEADER_MODEL_ALIAS_RESOLVED);
                    }
                }
                if !exposed_headers.is_empty() {
                    response_builder = response_builder
                        .header("Access-Control-Expose-Headers", exposed_headers.join(", "));
                }

                response_builder
                    .body(Body::from_stream(byte_stream))
                    .unwrap()
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (
                    status_code,
                    ResponseJson::<ErrorResponse>(domain_error.into()),
                )
                    .into_response()
            }
        }
    } else {
        match app_state
            .completion_service
            .create_chat_completion(service_request)
            .await
        {
            Ok(response_with_bytes) => {
                let inference_id = hash_inference_id_to_uuid(&response_with_bytes.response.id);
                let completion = chat_response_to_text_response(response_with_bytes.response);

                let body_bytes = match serde_json::to_vec(&completion) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to serialize text completion response");
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            ResponseJson(ErrorResponse::new(
                                "Failed to assemble response".to_string(),
                                "internal_server_error".to_string(),
                            )),
                        )
                            .into_response();
                    }
                };

                // Annotate alias-served responses (issue #573). This
                // endpoint already re-serializes (no raw-bytes contract),
                // so the warning injection costs nothing extra.
                let body_bytes = match &alias_canonical {
                    Some(canonical) => inject_warning_field(
                        &body_bytes,
                        &alias_warning_message(&request.model, canonical),
                    )
                    .unwrap_or(body_bytes),
                    None => body_bytes,
                };

                let mut response_builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(HEADER_INFERENCE_ID, inference_id.to_string());

                let mut exposed_headers: Vec<&str> = vec![HEADER_INFERENCE_ID];
                // Announce alias substitution so it is never silent (issue #573)
                if let Some(canonical) = &alias_canonical {
                    if let Ok(value) = header::HeaderValue::from_str(&format!(
                        "{} -> {}",
                        request.model, canonical
                    )) {
                        response_builder =
                            response_builder.header(HEADER_MODEL_ALIAS_RESOLVED, value);
                        exposed_headers.push(HEADER_MODEL_ALIAS_RESOLVED);
                    }
                }
                response_builder = response_builder
                    .header("Access-Control-Expose-Headers", exposed_headers.join(", "));

                response_builder.body(Body::from(body_bytes)).unwrap()
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (
                    status_code,
                    ResponseJson::<ErrorResponse>(domain_error.into()),
                )
                    .into_response()
            }
        }
    }
}

/// Reshape a non-streaming chat completion response into the legacy
/// `object: "text_completion"` format: each choice's assistant message content
/// becomes the choice `text`.
fn chat_response_to_text_response(
    response: inference_providers::ChatCompletionResponse,
) -> CompletionResponse {
    let cached = response.usage.cached_tokens();
    CompletionResponse {
        id: response.id,
        object: "text_completion".to_string(),
        created: response.created,
        model: response.model,
        choices: response
            .choices
            .into_iter()
            .map(|c| CompletionChoice {
                index: c.index,
                text: c.message.content.unwrap_or_default(),
                logprobs: None,
                finish_reason: c.finish_reason,
            })
            .collect(),
        usage: CompletionUsage {
            prompt_tokens: response.usage.prompt_tokens,
            prompt_tokens_details: (cached != 0).then_some(InputTokensDetails {
                cached_tokens: cached as i64,
            }),
            completion_tokens: response.usage.completion_tokens,
            completion_tokens_details: None,
            total_tokens: response.usage.total_tokens,
        },
    }
}

/// Reshape a streaming chat chunk into the legacy text-completion chunk format:
/// the assistant `delta.content` becomes the choice `text`.
fn chat_chunk_to_text_chunk(
    chunk: inference_providers::StreamChunk,
) -> inference_providers::models::CompletionChunk {
    let chat = match chunk {
        inference_providers::StreamChunk::Chat(c) => c,
        // The service emits chat chunks for this request shape; pass any
        // already-text chunk through unchanged.
        inference_providers::StreamChunk::Text(c) => return c,
    };
    inference_providers::models::CompletionChunk {
        id: chat.id,
        object: "text_completion".to_string(),
        created: chat.created,
        model: chat.model,
        system_fingerprint: chat.system_fingerprint,
        choices: chat
            .choices
            .into_iter()
            .map(|c| inference_providers::models::TextChoice {
                index: c.index,
                text: c.delta.and_then(|d| d.content).unwrap_or_default(),
                logprobs: None,
                finish_reason: c.finish_reason,
            })
            .collect(),
        usage: chat.usage,
    }
}

/// List available models
///
/// Returns all AI models available for completions. OpenAI-compatible endpoint.
#[utoipa::path(
    get,
    path = "/v1/models",
    tag = "Chat",
    responses(
        (status = 200, description = "List of available models", body = ModelsResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ()
    )
)]
pub async fn models(
    State(app_state): State<AppState>,
) -> Result<ResponseJson<ModelsResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Models list request");

    let models = app_state
        .models_service
        .get_models_with_pricing()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    e.to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    let response = ModelsResponse {
        object: "list".to_string(),
        data: models.into_iter().map(model_with_pricing_to_info).collect(),
    };
    Ok(ResponseJson(response))
}

/// Convert a nano-USD amount (DB scale 9 — used for per-token, per-image, and
/// per-request prices) to a USD string suitable for OpenRouter
/// (e.g. 8_000 → "0.000008"). Strings are required by the OpenRouter provider
/// spec to avoid float precision issues, so this uses pure integer arithmetic.
/// Pricing is non-negative throughout the system; a defensive `abs()` keeps
/// the formatter total in the unexpected case.
fn nano_dollars_to_per_token_string(nano_dollars: i64) -> String {
    if nano_dollars == 0 {
        return "0".to_string();
    }
    let n = nano_dollars.unsigned_abs();
    let dollars = n / 1_000_000_000;
    let nanos = n % 1_000_000_000;
    let mut s = if nanos == 0 {
        format!("{dollars}")
    } else {
        let frac = format!("{nanos:09}");
        let frac = frac.trim_end_matches('0');
        format!("{dollars}.{frac}")
    };
    if nano_dollars < 0 {
        s.insert(0, '-');
    }
    s
}

fn model_with_pricing_to_info(model: services::models::ModelWithPricing) -> ModelInfo {
    // Legacy HuggingFace-style fields: USD per million tokens.
    // nano_dollars_per_token * 0.001 = USD per million.
    let input_per_million = (model.input_cost_per_token as f64) * 0.001;
    let output_per_million = (model.output_cost_per_token as f64) * 0.001;

    let pricing = ModelPricing {
        input: input_per_million,
        output: output_per_million,
        prompt: nano_dollars_to_per_token_string(model.input_cost_per_token),
        completion: nano_dollars_to_per_token_string(model.output_cost_per_token),
        image: nano_dollars_to_per_token_string(model.cost_per_image),
        request: "0".to_string(),
        input_cache_read: nano_dollars_to_per_token_string(model.cache_read_cost_per_token),
    };

    // OpenRouter's provider spec marks `input_modalities` / `output_modalities`
    // as REQUIRED fields. They are derived from the nullable `architecture`
    // column, so models whose architecture was never backfilled would otherwise
    // emit no modality fields at all. Default to text/text so the required
    // fields are never absent. Real per-model values are set via the admin API.
    let input_modalities = model
        .input_modalities
        .unwrap_or_else(|| vec!["text".to_string()]);
    let output_modalities = model
        .output_modalities
        .unwrap_or_else(|| vec!["text".to_string()]);
    let architecture = Some(ModelArchitecture {
        input_modalities: input_modalities.clone(),
        output_modalities: output_modalities.clone(),
    });

    let name = if model.model_display_name.is_empty() {
        None
    } else {
        Some(model.model_display_name)
    };
    let description = if model.model_description.is_empty() {
        None
    } else {
        Some(model.model_description)
    };

    ModelInfo {
        id: model.model_name,
        object: "model".to_string(),
        created: model.created_at.timestamp(),
        owned_by: model.owned_by,
        name,
        hugging_face_id: model.hugging_face_id,
        quantization: model.quantization,
        pricing: Some(pricing),
        context_length: Some(model.context_length),
        max_output_length: model.max_output_length,
        architecture,
        input_modalities: Some(input_modalities),
        output_modalities: Some(output_modalities),
        supported_sampling_parameters: model.supported_sampling_parameters,
        supported_features: model.supported_features,
        is_ready: model.is_ready,
        deprecation_date: model
            .deprecation_date
            .as_ref()
            .map(crate::routes::admin::format_deprecation_date),
        description,
        top_provider: Some(TopProvider {
            context_length: Some(model.context_length),
            max_completion_tokens: model.max_output_length,
            is_moderated: false,
        }),
        datacenters: crate::models::Datacenter::from_codes(model.datacenters),
        // OpenRouter requires `id` to match its canonical slug, or an explicit
        // override via this nested object. Emit it only when an override is set
        // (NULL/empty → omit the key entirely).
        openrouter: model
            .openrouter_slug
            .filter(|s| !s.is_empty())
            .map(|slug| crate::models::OpenRouter { slug }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nano_dollars_zero_renders_as_bare_zero() {
        assert_eq!(nano_dollars_to_per_token_string(0), "0");
    }

    #[test]
    fn nano_dollars_one_keeps_full_nine_decimal_places() {
        // 1 nano-USD = $0.000000001 — boundary for our scale.
        assert_eq!(nano_dollars_to_per_token_string(1), "0.000000001");
    }

    #[test]
    fn nano_dollars_typical_per_token_price_trims_trailing_zeros() {
        // $0.000008 per token, e.g. Claude Sonnet input.
        assert_eq!(nano_dollars_to_per_token_string(8_000), "0.000008");
        // $0.000024 per token, e.g. Claude Sonnet output.
        assert_eq!(nano_dollars_to_per_token_string(24_000), "0.000024");
    }

    #[test]
    fn nano_dollars_whole_dollar_amount_omits_decimal_point() {
        // 1_000_000_000 nano-USD = $1 exactly. Must NOT render as "1." or "1.0".
        assert_eq!(nano_dollars_to_per_token_string(1_000_000_000), "1");
        assert_eq!(nano_dollars_to_per_token_string(5_000_000_000), "5");
    }

    #[test]
    fn nano_dollars_mixed_integer_and_fraction() {
        // $1.500000000 → "1.5" after trim.
        assert_eq!(nano_dollars_to_per_token_string(1_500_000_000), "1.5");
        // $1.000000001 → all 9 frac digits preserved (no trailing zeros to trim).
        assert_eq!(
            nano_dollars_to_per_token_string(1_000_000_001),
            "1.000000001"
        );
    }

    #[test]
    fn nano_dollars_max_i64_does_not_lose_precision() {
        // Integer arithmetic must remain exact at the upper bound, unlike the
        // previous f64-based implementation.
        // i64::MAX = 9_223_372_036_854_775_807 nano-USD
        //         = 9_223_372_036.854775807 USD
        assert_eq!(
            nano_dollars_to_per_token_string(i64::MAX),
            "9223372036.854775807"
        );
    }

    fn make_model_with_pricing(
        input_modalities: Option<Vec<String>>,
        output_modalities: Option<Vec<String>>,
    ) -> services::models::ModelWithPricing {
        services::models::ModelWithPricing {
            id: uuid::Uuid::new_v4(),
            model_name: "test/model".to_string(),
            model_display_name: "Test Model".to_string(),
            model_description: "A test model".to_string(),
            model_icon: None,
            input_cost_per_token: 0,
            output_cost_per_token: 0,
            cost_per_image: 0,
            cache_read_cost_per_token: 0,
            context_length: 4096,
            verifiable: false,
            aliases: vec![],
            owned_by: "test".to_string(),
            provider_type: "vllm".to_string(),
            provider_config: None,
            attestation_supported: false,
            input_modalities,
            output_modalities,
            inference_url: None,
            hugging_face_id: None,
            quantization: None,
            max_output_length: None,
            supported_sampling_parameters: vec![],
            supported_features: vec![],
            datacenters: None,
            is_ready: None,
            deprecation_date: None,
            openrouter_slug: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn model_without_architecture_defaults_to_text_modalities() {
        // OpenRouter requires input_modalities / output_modalities. Models whose
        // architecture column was never backfilled (NULL modalities) must still
        // emit the text/text defaults so the required fields are never absent.
        let info = model_with_pricing_to_info(make_model_with_pricing(None, None));

        assert_eq!(info.input_modalities, Some(vec!["text".to_string()]));
        assert_eq!(info.output_modalities, Some(vec!["text".to_string()]));

        let architecture = info
            .architecture
            .expect("architecture must be populated even without DB modalities");
        assert_eq!(architecture.input_modalities, vec!["text".to_string()]);
        assert_eq!(architecture.output_modalities, vec!["text".to_string()]);
    }

    #[test]
    fn model_with_architecture_preserves_real_modalities() {
        // When the DB has real modalities they must pass through untouched
        // (both the flat fields and the nested architecture shape).
        let info = model_with_pricing_to_info(make_model_with_pricing(
            Some(vec!["text".to_string(), "image".to_string()]),
            Some(vec!["text".to_string()]),
        ));

        assert_eq!(
            info.input_modalities,
            Some(vec!["text".to_string(), "image".to_string()])
        );
        assert_eq!(info.output_modalities, Some(vec!["text".to_string()]));

        let architecture = info.architecture.expect("architecture must be populated");
        assert_eq!(
            architecture.input_modalities,
            vec!["text".to_string(), "image".to_string()]
        );
        assert_eq!(architecture.output_modalities, vec!["text".to_string()]);
    }

    #[test]
    fn model_without_openrouter_slug_omits_nested_object() {
        // No override set → the public ModelInfo must not carry the nested
        // `openrouter` object at all (serde skips it when None).
        let info = model_with_pricing_to_info(make_model_with_pricing(None, None));
        assert!(
            info.openrouter.is_none(),
            "openrouter object must be omitted when no slug override is set"
        );
        // And it must not appear in the serialized JSON either.
        let json = serde_json::to_value(&info).unwrap();
        assert!(
            json.get("openrouter").is_none(),
            "serialized JSON must omit the openrouter key when unset"
        );
    }

    #[test]
    fn model_with_openrouter_slug_emits_nested_object() {
        // Override set → the public ModelInfo must carry
        // `openrouter: { slug: <value> }`.
        let mut model = make_model_with_pricing(None, None);
        model.openrouter_slug = Some("z-ai/glm-5.1".to_string());
        let info = model_with_pricing_to_info(model);
        let openrouter = info
            .openrouter
            .as_ref()
            .expect("openrouter object must be present when slug override is set");
        assert_eq!(openrouter.slug, "z-ai/glm-5.1");

        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json["openrouter"]["slug"],
            serde_json::json!("z-ai/glm-5.1"),
            "serialized JSON must nest the slug under openrouter.slug"
        );
    }

    fn make_chat_chunk(id: &str) -> inference_providers::StreamChunk {
        inference_providers::StreamChunk::Chat(inference_providers::models::ChatCompletionChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "test-model".to_string(),
            system_fingerprint: None,
            choices: vec![],
            usage: None,
            prompt_token_ids: None,
            modality: None,
            extra: Default::default(),
        })
    }

    fn chat_request_with_include_usage(include_usage: Option<bool>) -> ChatCompletionRequest {
        let mut extra = std::collections::HashMap::new();
        if let Some(include_usage) = include_usage {
            extra.insert(
                "stream_options".to_string(),
                serde_json::json!({ "include_usage": include_usage }),
            );
        }
        ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            n: None,
            stream: Some(true),
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            extra,
        }
    }

    fn chat_stream_chunk_with_usage(
        choices: Vec<inference_providers::models::ChatChoice>,
    ) -> inference_providers::models::ChatCompletionChunk {
        inference_providers::models::ChatCompletionChunk {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "test-model".to_string(),
            system_fingerprint: None,
            choices,
            usage: Some(inference_providers::models::TokenUsage::new(10, 5)),
            prompt_token_ids: None,
            modality: None,
            extra: Default::default(),
        }
    }

    fn chat_stream_content_choice() -> inference_providers::models::ChatChoice {
        inference_providers::models::ChatChoice {
            index: 0,
            delta: Some(inference_providers::models::ChatDelta {
                role: None,
                content: Some("hello".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                extra: Default::default(),
            }),
            logprobs: None,
            finish_reason: None,
            token_ids: None,
        }
    }

    fn chat_stream_finish_choice() -> inference_providers::models::ChatChoice {
        inference_providers::models::ChatChoice {
            finish_reason: Some(inference_providers::models::FinishReason::Stop),
            ..chat_stream_content_choice()
        }
    }

    #[test]
    fn chat_stream_include_usage_defaults_to_false() {
        let request = chat_request_with_include_usage(None);
        assert!(!chat_stream_include_usage_requested(&request));

        let request = chat_request_with_include_usage(Some(false));
        assert!(!chat_stream_include_usage_requested(&request));

        let request = chat_request_with_include_usage(Some(true));
        assert!(chat_stream_include_usage_requested(&request));
    }

    #[test]
    fn chat_stream_continuous_usage_is_detected() {
        let mut request = chat_request_with_include_usage(Some(true));
        request.extra.insert(
            "stream_options".to_string(),
            serde_json::json!({
                "include_usage": true,
                "continuous_usage_stats": true
            }),
        );

        assert!(chat_stream_continuous_usage_requested(&request));
    }

    #[test]
    fn include_usage_rewrites_and_signs_attested_streams() {
        let request = chat_request_with_include_usage(Some(true));
        let mode = chat_stream_usage_mode(&request, Some(true), false);

        assert!(mode.rewrite_public_stream_usage);
        assert!(mode.gateway_signature_enabled);
    }

    #[test]
    fn include_usage_rewrites_non_attested_without_gateway_signature() {
        let request = chat_request_with_include_usage(Some(true));
        let mode = chat_stream_usage_mode(&request, Some(false), false);

        assert!(mode.rewrite_public_stream_usage);
        assert!(!mode.gateway_signature_enabled);
    }

    #[test]
    fn default_attested_stream_preserves_provider_passthrough() {
        let request = chat_request_with_include_usage(None);
        let mode = chat_stream_usage_mode(&request, Some(true), false);

        assert!(!mode.rewrite_public_stream_usage);
        assert!(!mode.gateway_signature_enabled);
    }

    #[test]
    fn include_usage_false_attested_stream_preserves_provider_passthrough() {
        let request = chat_request_with_include_usage(Some(false));
        let mode = chat_stream_usage_mode(&request, Some(true), false);

        assert!(!mode.rewrite_public_stream_usage);
        assert!(!mode.gateway_signature_enabled);
    }

    #[test]
    fn default_non_attested_stream_preserves_provider_passthrough() {
        let request = chat_request_with_include_usage(None);
        let mode = chat_stream_usage_mode(&request, Some(false), false);

        assert!(!mode.rewrite_public_stream_usage);
        assert!(!mode.gateway_signature_enabled);
    }

    #[test]
    fn continuous_usage_stats_is_passthrough_when_model_metadata_is_missing() {
        let mut request = chat_request_with_include_usage(None);
        request.extra.insert(
            "stream_options".to_string(),
            serde_json::json!({ "continuous_usage_stats": true }),
        );
        let mode = chat_stream_usage_mode(&request, None, false);

        assert!(!mode.rewrite_public_stream_usage);
        assert!(!mode.gateway_signature_enabled);
    }

    #[test]
    fn continuous_usage_stats_opts_out_of_rewrite() {
        let mut request = chat_request_with_include_usage(Some(true));
        request.extra.insert(
            "stream_options".to_string(),
            serde_json::json!({
                "include_usage": true,
                "continuous_usage_stats": true
            }),
        );

        let passthrough = chat_stream_usage_mode(&request, Some(true), false);
        assert!(!passthrough.rewrite_public_stream_usage);
        assert!(!passthrough.gateway_signature_enabled);

        let e2ee_passthrough = chat_stream_usage_mode(&request, Some(true), true);
        assert!(!e2ee_passthrough.rewrite_public_stream_usage);

        request.extra.insert(
            "modalities".to_string(),
            serde_json::json!(["text", "audio"]),
        );
        let audio_passthrough = chat_stream_usage_mode(&request, Some(true), false);
        assert!(!audio_passthrough.rewrite_public_stream_usage);
    }

    #[test]
    fn chat_stream_non_text_modalities_preserve_raw_passthrough() {
        let mut request = chat_request_with_include_usage(None);
        assert!(!chat_stream_has_non_text_modalities(&request));

        request.extra.insert(
            "modalities".to_string(),
            serde_json::json!(["text", "audio"]),
        );
        assert!(chat_stream_has_non_text_modalities(&request));

        request
            .extra
            .insert("modalities".to_string(), serde_json::json!(["TEXT"]));
        assert!(!chat_stream_has_non_text_modalities(&request));
    }

    #[test]
    fn default_chat_stream_suppresses_provider_usage_but_serializes_null() {
        let mut chunk = chat_stream_chunk_with_usage(vec![chat_stream_content_choice()]);

        assert!(prepare_chat_stream_chunk_for_client(&mut chunk, false));
        assert!(chunk.usage.is_none());

        let serialized = serde_json::to_value(&chunk).expect("chunk should serialize");
        assert!(
            serialized
                .get("usage")
                .is_some_and(serde_json::Value::is_null),
            "ordinary chunks should carry usage:null instead of provider usage"
        );
    }

    #[test]
    fn default_chat_stream_drops_provider_usage_only_chunk() {
        let mut chunk = chat_stream_chunk_with_usage(vec![]);

        assert!(!prepare_chat_stream_chunk_for_client(&mut chunk, false));
    }

    #[test]
    fn include_usage_records_usage_for_synthetic_final_chunk() {
        let mut final_usage = None;
        let mut content_chunk = chat_stream_chunk_with_usage(vec![chat_stream_content_choice()]);
        assert!(prepare_chat_stream_chunk_for_client_with_state(
            &mut content_chunk,
            true,
            &mut final_usage
        ));
        assert!(content_chunk.usage.is_none());
        assert_eq!(
            final_usage.as_ref().map(|usage| usage.total_tokens),
            Some(15)
        );

        let content_json = serde_json::to_value(&content_chunk).expect("chunk should serialize");
        assert!(
            content_json
                .get("usage")
                .is_some_and(serde_json::Value::is_null),
            "intermediate chunks should carry usage:null"
        );

        let mut final_choice_chunk =
            chat_stream_chunk_with_usage(vec![chat_stream_finish_choice()]);
        final_choice_chunk.usage = Some(inference_providers::TokenUsage::new(10, 7));
        assert!(prepare_chat_stream_chunk_for_client_with_state(
            &mut final_choice_chunk,
            true,
            &mut final_usage
        ));
        assert!(final_choice_chunk.usage.is_none());
        assert_eq!(
            final_usage.as_ref().map(|usage| usage.total_tokens),
            Some(17)
        );

        let mut usage_only_chunk = chat_stream_chunk_with_usage(vec![]);
        usage_only_chunk.usage = Some(inference_providers::TokenUsage::new(10, 9));
        assert!(!prepare_chat_stream_chunk_for_client_with_state(
            &mut usage_only_chunk,
            true,
            &mut final_usage
        ));
        assert_eq!(
            final_usage.as_ref().map(|usage| usage.total_tokens),
            Some(19)
        );
    }

    #[test]
    fn include_usage_preserves_converter_only_usage_for_tail() {
        let mut final_usage = None;
        let mut final_choice_chunk =
            chat_stream_chunk_with_usage(vec![chat_stream_finish_choice()]);
        assert!(prepare_chat_stream_chunk_for_client_with_state(
            &mut final_choice_chunk,
            true,
            &mut final_usage
        ));
        assert!(final_choice_chunk.usage.is_none());
        assert_eq!(
            final_usage.as_ref().map(|usage| usage.total_tokens),
            Some(15)
        );
    }

    #[test]
    fn final_usage_chunk_uses_stream_template_metadata() {
        let template = Some((
            "chatcmpl-test".to_string(),
            "test-model".to_string(),
            1234567890,
            Some("fp-test".to_string()),
        ));

        let bytes =
            build_final_usage_chunk_bytes(inference_providers::TokenUsage::new(10, 5), &template)
                .expect("final usage chunk should serialize")
                .expect("template should produce final usage chunk");

        let body = String::from_utf8(bytes.to_vec()).expect("SSE bytes should be UTF-8");
        assert!(body.starts_with("data: "));
        assert!(body.ends_with("\n\n"));
        let payload = body
            .trim_start_matches("data: ")
            .trim_end()
            .trim_end_matches('\n');
        let value: serde_json::Value =
            serde_json::from_str(payload).expect("final usage payload should be JSON");
        assert_eq!(value["id"], "chatcmpl-test");
        assert_eq!(value["model"], "test-model");
        assert_eq!(value["created"], 1234567890);
        assert_eq!(value["system_fingerprint"], "fp-test");
        assert!(value["choices"].as_array().is_some_and(Vec::is_empty));
        assert_eq!(value["usage"]["prompt_tokens"], 10);
        assert_eq!(value["usage"]["completion_tokens"], 5);
    }

    #[test]
    fn rewritten_control_events_keep_comments_and_drop_separators() {
        let blank = inference_providers::SSEEvent {
            raw_bytes: Bytes::from_static(b"\n"),
            chunk: None,
            raw_passthrough: true,
        };
        assert!(rewritten_control_event_bytes(&blank).is_none());

        let comment = inference_providers::SSEEvent {
            raw_bytes: Bytes::from_static(b": keepalive\n"),
            chunk: None,
            raw_passthrough: true,
        };
        assert_eq!(
            rewritten_control_event_bytes(&comment),
            Some(Bytes::from_static(b": keepalive\n\n"))
        );

        let done = inference_providers::SSEEvent {
            raw_bytes: Bytes::from_static(b"data: [DONE]\n"),
            chunk: None,
            raw_passthrough: true,
        };
        assert!(rewritten_control_event_bytes(&done).is_none());
    }

    #[test]
    fn default_chat_stream_strips_usage_from_terminal_choice_chunk() {
        let mut final_choice_chunk =
            chat_stream_chunk_with_usage(vec![chat_stream_finish_choice()]);

        assert!(prepare_chat_stream_chunk_for_client(
            &mut final_choice_chunk,
            false
        ));
        assert!(final_choice_chunk.usage.is_none());
    }

    #[test]
    fn test_extract_inference_id_from_chunk_valid() {
        let chunk = make_chat_chunk("chatcmpl-123abc");
        let uuid1 = extract_inference_id_from_chunk(&chunk);
        // UUID should be deterministic - same input produces same UUID
        let uuid2 = extract_inference_id_from_chunk(&chunk);
        assert_eq!(uuid1, uuid2);
    }

    #[test]
    fn test_extract_inference_id_from_chunk_deterministic() {
        let chunk1 = make_chat_chunk("chatcmpl-test123");
        let chunk2 = make_chat_chunk("chatcmpl-test123");
        let uuid1 = extract_inference_id_from_chunk(&chunk1);
        let uuid2 = extract_inference_id_from_chunk(&chunk2);
        assert_eq!(uuid1, uuid2);
    }

    #[test]
    fn test_extract_inference_id_from_chunk_different_ids() {
        let chunk1 = make_chat_chunk("chatcmpl-abc123");
        let chunk2 = make_chat_chunk("chatcmpl-xyz789");
        let uuid1 = extract_inference_id_from_chunk(&chunk1);
        let uuid2 = extract_inference_id_from_chunk(&chunk2);
        assert_ne!(uuid1, uuid2);
    }

    #[test]
    fn test_extract_inference_id_from_chunk_empty_id() {
        let chunk = make_chat_chunk("");
        let result = extract_inference_id_from_chunk(&chunk);
        // Empty string should still produce a valid UUID
        assert!(
            !result.is_nil(),
            "empty provider ID should still produce a non-nil UUID"
        );
    }

    fn empty_delta() -> inference_providers::models::ChatDelta {
        inference_providers::models::ChatDelta {
            role: None,
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            extra: Default::default(),
        }
    }

    fn make_chat_chunk_with_choice(
        delta: Option<inference_providers::models::ChatDelta>,
        finish_reason: Option<inference_providers::models::FinishReason>,
    ) -> inference_providers::StreamChunk {
        inference_providers::StreamChunk::Chat(inference_providers::models::ChatCompletionChunk {
            id: "x".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "m".to_string(),
            system_fingerprint: None,
            choices: vec![inference_providers::models::ChatChoice {
                index: 0,
                delta,
                logprobs: None,
                finish_reason,
                token_ids: None,
            }],
            usage: None,
            prompt_token_ids: None,
            modality: None,
            extra: Default::default(),
        })
    }

    #[test]
    fn finish_chunk_with_empty_delta_drains_held_tail_in_place() {
        // Reproduces the bug Pierre flagged on PR #599:
        // when a content chunk pushes bytes into the per-choice tail and
        // the next chunk is a finish chunk with `delta: {}` + `finish_reason`,
        // the previous code only drained states whose corresponding delta
        // field was present on the finish chunk. The held tail then leaked
        // out *after* the finish chunk via build_flush_chunks — invisible
        // to clients that stop reading on finish_reason.
        //
        // Fix: when finish_reason is present, drain ALL per-choice states
        // into the finish chunk's delta in place, even if the delta field
        // was absent. Assert: (a) the tail is on the finish chunk;
        // (b) `states.content` is empty so build_flush_chunks emits nothing.
        use services::auto_redact::RedactionMap;

        let mut m = RedactionMap::new();
        m.lookup_or_mint("private_email", "alice@example.com", |_| false);
        let map = Arc::new(m);
        let mut states = StreamUnredactStates::default();

        // Chunk 1: content shorter than hold_size — sits in the tail.
        let mut chunk1 = make_chat_chunk_with_choice(
            Some(inference_providers::models::ChatDelta {
                content: Some("hi redact".to_string()),
                ..empty_delta()
            }),
            None,
        );
        unredact_chunk_in_place(&mut chunk1, &mut states, &map);

        // Chunk 2: empty delta + finish_reason.
        let mut chunk2 = make_chat_chunk_with_choice(
            Some(empty_delta()),
            Some(inference_providers::models::FinishReason::Stop),
        );
        unredact_chunk_in_place(&mut chunk2, &mut states, &map);

        let inference_providers::StreamChunk::Chat(c2) = &chunk2 else {
            panic!("expected Chat chunk");
        };
        let drained_content = c2.choices[0]
            .delta
            .as_ref()
            .and_then(|d| d.content.as_deref())
            .unwrap_or("");
        assert_eq!(
            drained_content, "hi redact",
            "held tail must be attached to the finish chunk's delta.content, not emitted after",
        );
        assert!(
            !states.content.contains_key(&0),
            "after drain the state must be removed so build_flush_chunks emits nothing for this choice",
        );
    }

    #[test]
    fn finish_chunk_with_empty_delta_drains_tool_call_arguments_in_place() {
        // Same bug as above, for tool-call arguments. A tool_calls fragment
        // leaves a partial JSON in the per-(choice,tc_idx) state; the next
        // chunk carries `finish_reason: tool_calls` with empty delta. The
        // held bytes must come out on THAT chunk, not after.
        use services::auto_redact::RedactionMap;

        let mut m = RedactionMap::new();
        m.lookup_or_mint("private_email", "alice@example.com", |_| false);
        let map = Arc::new(m);
        let mut states = StreamUnredactStates::default();

        // Chunk 1: tool_call args fragment that fits inside hold_size.
        let mut chunk1 = make_chat_chunk_with_choice(
            Some(inference_providers::models::ChatDelta {
                tool_calls: Some(vec![inference_providers::models::ToolCallDelta {
                    id: None,
                    type_: None,
                    index: Some(0),
                    function: Some(inference_providers::models::FunctionCallDelta {
                        name: None,
                        arguments: Some(r#"{"to":"x"}"#.to_string()),
                    }),
                    thought_signature: None,
                }]),
                ..empty_delta()
            }),
            None,
        );
        unredact_chunk_in_place(&mut chunk1, &mut states, &map);

        // Chunk 2: empty delta + finish_reason=tool_calls.
        let mut chunk2 = make_chat_chunk_with_choice(
            Some(empty_delta()),
            Some(inference_providers::models::FinishReason::ToolCalls),
        );
        unredact_chunk_in_place(&mut chunk2, &mut states, &map);

        let inference_providers::StreamChunk::Chat(c2) = &chunk2 else {
            panic!("expected Chat chunk");
        };
        let tool_calls = c2.choices[0]
            .delta
            .as_ref()
            .and_then(|d| d.tool_calls.as_ref())
            .expect("finish chunk must carry the drained tool_calls");
        assert_eq!(tool_calls.len(), 1);
        let args = tool_calls[0]
            .function
            .as_ref()
            .and_then(|f| f.arguments.as_deref())
            .unwrap_or("");
        assert_eq!(args, r#"{"to":"x"}"#);
        assert!(
            states.tool_call_arguments.is_empty(),
            "after drain the tool_call state must be removed",
        );
    }

    #[test]
    fn test_classify_provider_error_404_surfaces_message() {
        let (status, error_type, message) =
            classify_provider_error(404, "model 'foo' not found".to_string());
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error_type, "not_found_error");
        assert_eq!(message, "model 'foo' not found");
    }

    #[test]
    fn test_classify_provider_error_429_surfaces_message() {
        let (status, error_type, message) =
            classify_provider_error(429, "too many concurrent requests".to_string());
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error_type, "rate_limit_error");
        assert_eq!(message, "too many concurrent requests");
    }

    #[test]
    fn test_classify_provider_error_400_surfaces_message() {
        let (status, error_type, message) = classify_provider_error(
            400,
            "dimensions is not supported for this model".to_string(),
        );
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_type, "invalid_request_error");
        assert_eq!(message, "dimensions is not supported for this model");
    }

    #[test]
    fn test_classify_provider_error_422_preserves_upstream_status() {
        let (status, error_type, message) =
            classify_provider_error(422, "validation failed".to_string());
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error_type, "invalid_request_error");
        assert_eq!(message, "validation failed");
    }

    #[test]
    fn test_classify_provider_error_401_masked_as_5xx() {
        // 401 from upstream means *our* credentials are wrong — the client
        // did nothing to cause it, so we must not echo the auth error.
        let (status, error_type, message) =
            classify_provider_error(401, "Invalid API key 'sk-***'".to_string());
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error_type, "server_error");
        assert!(
            !message.contains("sk-"),
            "must not leak upstream credentials in surfaced message"
        );
        assert!(message.contains("try again later"));
    }

    #[test]
    fn test_classify_provider_error_403_masked_as_5xx() {
        let (status, error_type, message) =
            classify_provider_error(403, "Forbidden: backend ACL denied".to_string());
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error_type, "server_error");
        assert_eq!(
            message,
            "Embeddings request failed. Please try again later."
        );
    }

    #[test]
    fn test_classify_provider_error_407_masked_as_5xx() {
        let (status, error_type, _) =
            classify_provider_error(407, "proxy auth required".to_string());
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error_type, "server_error");
    }

    #[test]
    fn test_classify_provider_error_5xx_masked() {
        // 5xx bodies may contain stack traces or internal details — never echo.
        let (status, error_type, message) = classify_provider_error(
            502,
            "RuntimeError: traceback...internal-host:9000".to_string(),
        );
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error_type, "server_error");
        assert!(!message.contains("traceback"));
        assert!(!message.contains("internal-host"));
    }

    #[test]
    fn test_stream_chunk_serialization_preserves_field_order() {
        // Verify that StreamChunk::Chat serializes with struct field order
        // (not alphabetical), matching what serde_json::to_string produces.
        // The server uses to_string(&StreamChunk) and the mock must match.
        let chunk = inference_providers::models::ChatCompletionChunk {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "test-model".to_string(),
            system_fingerprint: None,
            choices: vec![],
            usage: Some(inference_providers::models::TokenUsage::new(10, 5)),
            prompt_token_ids: None,
            modality: None,
            extra: Default::default(),
        };

        let stream_chunk = inference_providers::StreamChunk::Chat(chunk.clone());

        // Both serialization paths must produce identical output
        let from_inner = serde_json::to_string(&chunk).expect("inner chunk should serialize");
        let from_enum =
            serde_json::to_string(&stream_chunk).expect("enum-wrapped chunk should serialize");
        assert_eq!(from_inner, from_enum);

        // Field order should be struct order, not alphabetical
        let id_pos = from_inner
            .find("\"id\"")
            .expect("serialized chunk should contain id field");
        let choices_pos = from_inner
            .find("\"choices\"")
            .expect("serialized chunk should contain choices field");
        assert!(
            id_pos < choices_pos,
            "id should appear before choices (struct field order)"
        );
    }

    #[test]
    fn test_sse_error_frame_is_valid_json() {
        // Every stream-error variant must produce a frame whose `data:` payload
        // parses as JSON of shape {"error": {"message": ..., "type": ...}}.
        // The historical `data: error: <msg>\n\n` format broke clients that
        // parse the data payload as JSON (opencode, vercel/ai-sdk).
        let cases = vec![
            inference_providers::CompletionError::CompletionError("boom".into()),
            inference_providers::CompletionError::HttpError {
                status_code: 503,
                message: "overloaded".into(),
                is_external: false,
            },
            inference_providers::CompletionError::HttpError {
                status_code: 429,
                message: "rate limit".into(),
                is_external: false,
            },
            inference_providers::CompletionError::HttpError {
                status_code: 400,
                message: "bad request".into(),
                is_external: false,
            },
            inference_providers::CompletionError::InvalidResponse("Failed to parse event".into()),
            inference_providers::CompletionError::NoPubKeyProvider("abc".into()),
            inference_providers::CompletionError::Unknown("mystery".into()),
            inference_providers::CompletionError::Timeout {
                operation: "completion".into(),
                timeout_seconds: 30,
            },
        ];

        for e in &cases {
            let frame = sse_error_frame(e);
            let text = std::str::from_utf8(&frame).expect("frame is utf-8");
            assert!(
                text.starts_with("data: "),
                "frame missing 'data: ' prefix: {text:?}"
            );
            assert!(
                text.ends_with("\n\n"),
                "frame missing SSE terminator: {text:?}"
            );
            let payload = text
                .strip_prefix("data: ")
                .and_then(|s| s.strip_suffix("\n\n"))
                .expect("frame must have data: prefix and \\n\\n suffix");
            let json: serde_json::Value = serde_json::from_str(payload).unwrap_or_else(|err| {
                panic!("frame payload not valid JSON for {e:?}: err={err}, payload={payload}")
            });
            let obj = json.get("error").expect("payload has error key");
            assert!(obj
                .get("message")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()));
            assert!(obj
                .get("type")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()));
        }
    }

    #[test]
    fn test_completion_stream_error_openai_type_http_status_mapping() {
        let rate_limited = inference_providers::CompletionError::HttpError {
            status_code: 429,
            message: "rl".into(),
            is_external: false,
        };
        assert_eq!(
            completion_stream_error_openai_type(&rate_limited),
            "rate_limit_exceeded"
        );

        let client_err = inference_providers::CompletionError::HttpError {
            status_code: 400,
            message: "bad".into(),
            is_external: false,
        };
        assert_eq!(
            completion_stream_error_openai_type(&client_err),
            "invalid_request_error"
        );

        let server_err = inference_providers::CompletionError::HttpError {
            status_code: 503,
            message: "down".into(),
            is_external: false,
        };
        assert_eq!(
            completion_stream_error_openai_type(&server_err),
            "server_error"
        );

        let parse_err = inference_providers::CompletionError::InvalidResponse("bad chunk".into());
        assert_eq!(
            completion_stream_error_openai_type(&parse_err),
            "server_error"
        );
    }

    #[test]
    fn chat_response_to_text_response_maps_content_and_usage() {
        let response: inference_providers::ChatCompletionResponse =
            serde_json::from_value(serde_json::json!({
                "id": "cmpl-abc",
                "object": "chat.completion",
                "created": 42,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "The capital is Paris."},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 11,
                    "completion_tokens": 5,
                    "total_tokens": 16,
                    "prompt_tokens_details": {"cached_tokens": 4}
                }
            }))
            .unwrap();

        let text = chat_response_to_text_response(response);

        assert_eq!(text.object, "text_completion");
        assert_eq!(text.id, "cmpl-abc");
        assert_eq!(text.created, 42);
        assert_eq!(text.choices.len(), 1);
        assert_eq!(text.choices[0].index, 0);
        assert_eq!(text.choices[0].text, "The capital is Paris.");
        assert_eq!(text.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(text.usage.prompt_tokens, 11);
        assert_eq!(text.usage.completion_tokens, 5);
        assert_eq!(text.usage.total_tokens, 16);
        assert_eq!(
            text.usage.prompt_tokens_details.map(|d| d.cached_tokens),
            Some(4)
        );
    }

    #[test]
    fn chat_response_to_text_response_handles_missing_content_and_no_cache() {
        let response: inference_providers::ChatCompletionResponse =
            serde_json::from_value(serde_json::json!({
                "id": "cmpl-empty",
                "object": "chat.completion",
                "created": 0,
                "model": "m",
                "choices": [{"index": 0, "message": {"role": "assistant"}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 0, "total_tokens": 1}
            }))
            .unwrap();

        let text = chat_response_to_text_response(response);

        assert_eq!(text.choices[0].text, "");
        assert!(text.choices[0].finish_reason.is_none());
        // No cached tokens reported -> details omitted.
        assert!(text.usage.prompt_tokens_details.is_none());
    }

    #[test]
    fn chat_chunk_to_text_chunk_maps_delta_content() {
        let chunk = make_chat_chunk_with_choice(
            Some(inference_providers::models::ChatDelta {
                content: Some("Hello".to_string()),
                ..empty_delta()
            }),
            Some(inference_providers::models::FinishReason::Stop),
        );

        let text = chat_chunk_to_text_chunk(chunk);

        assert_eq!(text.object, "text_completion");
        assert_eq!(text.choices.len(), 1);
        assert_eq!(text.choices[0].index, 0);
        assert_eq!(text.choices[0].text, "Hello");
        assert!(matches!(
            text.choices[0].finish_reason,
            Some(inference_providers::models::FinishReason::Stop)
        ));
    }

    #[test]
    fn chat_chunk_to_text_chunk_empty_delta_yields_empty_text() {
        let chunk = make_chat_chunk_with_choice(Some(empty_delta()), None);
        let text = chat_chunk_to_text_chunk(chunk);
        assert_eq!(text.choices[0].text, "");
        assert!(text.choices[0].finish_reason.is_none());
    }

    #[test]
    fn completion_request_deserializes_minimal_body() {
        // `extra` is #[serde(flatten)]: a request with only model+prompt must
        // parse (no required "extra" object) and leave `extra` empty.
        let req: CompletionRequest =
            serde_json::from_str(r#"{"model":"m","prompt":"hello"}"#).unwrap();
        assert_eq!(req.model, "m");
        assert_eq!(req.prompt.single_text().unwrap(), "hello");
        assert!(req.extra.is_empty());
    }

    #[test]
    fn completion_request_flattens_unknown_fields_into_extra() {
        let req: CompletionRequest =
            serde_json::from_str(r#"{"model":"m","prompt":"hi","auto_redact":true}"#).unwrap();
        assert_eq!(
            req.extra.get("auto_redact"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    fn parse_completion_request(json: &str) -> CompletionRequest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn unsupported_params_are_rejected() {
        assert_eq!(
            unsupported_completion_param(&parse_completion_request(
                r#"{"model":"m","prompt":"p","echo":true}"#
            )),
            Some("echo")
        );
        assert_eq!(
            unsupported_completion_param(&parse_completion_request(
                r#"{"model":"m","prompt":"p","logprobs":5}"#
            )),
            Some("logprobs")
        );
        assert_eq!(
            unsupported_completion_param(&parse_completion_request(
                r#"{"model":"m","prompt":"p","best_of":3}"#
            )),
            Some("best_of")
        );
    }

    #[test]
    fn supported_and_default_params_are_accepted() {
        // echo:false, best_of:1, and penalties are not rejected.
        assert_eq!(
            unsupported_completion_param(&parse_completion_request(
                r#"{"model":"m","prompt":"p","echo":false,"best_of":1,"presence_penalty":0.5,"frequency_penalty":-0.2}"#
            )),
            None
        );
        assert_eq!(
            unsupported_completion_param(&parse_completion_request(
                r#"{"model":"m","prompt":"p"}"#
            )),
            None
        );
    }

    #[test]
    fn convert_forwards_penalties_into_extra() {
        let req = parse_completion_request(
            r#"{"model":"m","prompt":"p","presence_penalty":0.5,"frequency_penalty":-0.2}"#,
        );
        let body_hash = RequestBodyHash {
            hash: String::new(),
            body_bytes: Bytes::new(),
        };
        let svc = convert_text_request_to_service(
            &req,
            "p".to_string(),
            Uuid::nil(),
            "key".to_string(),
            Uuid::nil(),
            Uuid::nil(),
            body_hash,
            Uuid::nil(),
        );
        // Compare with tolerance: the typed field is f32, so the forwarded JSON
        // number widens to f64 (e.g. -0.2f32 -> -0.20000000298).
        let presence = svc
            .extra
            .get("presence_penalty")
            .and_then(|v| v.as_f64())
            .unwrap();
        let frequency = svc
            .extra
            .get("frequency_penalty")
            .and_then(|v| v.as_f64())
            .unwrap();
        assert!((presence - 0.5).abs() < 1e-6);
        assert!((frequency + 0.2).abs() < 1e-6);
    }

    #[test]
    fn stop_accepts_string_or_array() {
        // Single string form (e.g. {"stop":"\n"}) must deserialize, not 422.
        let single = parse_completion_request(r#"{"model":"m","prompt":"p","stop":"\n"}"#);
        assert_eq!(single.stop.unwrap().into_vec(), vec!["\n".to_string()]);

        let many = parse_completion_request(r#"{"model":"m","prompt":"p","stop":["a","b"]}"#);
        assert_eq!(
            many.stop.unwrap().into_vec(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn prompt_array_shapes_deserialize_without_422() {
        // The point: these all parse (so the handler can return a clean 400),
        // rather than failing JSON extraction at the framework layer.
        let single_elem = parse_completion_request(r#"{"model":"m","prompt":["solo"]}"#);
        assert_eq!(single_elem.prompt.single_text().unwrap(), "solo");

        let batch = parse_completion_request(r#"{"model":"m","prompt":["a","b"]}"#);
        assert!(batch.prompt.single_text().is_err());

        let tokens = parse_completion_request(r#"{"model":"m","prompt":[1,2,3]}"#);
        assert!(tokens.prompt.single_text().is_err());

        let token_batches = parse_completion_request(r#"{"model":"m","prompt":[[1,2],[3,4]]}"#);
        assert!(token_batches.prompt.single_text().is_err());
    }
}

/// Generate images from text prompt
///
/// Generate images using an AI model from a text description. OpenAI-compatible endpoint.
#[utoipa::path(
    post,
    path = "/v1/images/generations",
    tag = "Images",
    request_body = ImageGenerationRequest,
    responses(
        (status = 200, description = "Image generated successfully", body = ImageGenerationResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 529, description = "All inference backends overloaded; retry with exponential backoff", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn image_generations(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    OpenAiJson(request): OpenAiJson<crate::models::ImageGenerationRequest>,
) -> axum::response::Response {
    debug!(
        "Image generation request from api key: {:?}",
        api_key.api_key.id
    );
    debug!(
        "Image generation request: model={}, org={}, workspace={}",
        request.model, api_key.organization.id, api_key.workspace.id.0
    );

    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                error,
                "invalid_request_error".to_string(),
            )),
        )
            .into_response();
    }

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    // Resolve model to get UUID for usage tracking
    let model = match app_state
        .models_service
        .get_model_by_name(&request.model)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", request.model),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::debug!(error = %e, "Failed to resolve model for image generation");
            tracing::warn!("Image generation: model resolution failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    // Validate and enforce response_format for verifiable models
    // Verifiable models (attestation_supported = true) only support "b64_json" format
    // Default to "b64_json" if not specified to prevent downstream server from applying "url" default
    let response_format = if model.attestation_supported {
        match &request.response_format {
            Some(format) if format == "b64_json" => Some(format.clone()),
            Some(format) => {
                return (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!(
                            "response_format '{}' is not supported for verifiable models. Only 'b64_json' is supported.",
                            format
                        ),
                        "invalid_request_error".to_string(),
                    )),
                )
                    .into_response();
            }
            None => Some("b64_json".to_string()), // Default to b64_json for verifiable models
        }
    } else {
        request.response_format.clone()
    };

    // Convert API request to provider params
    let mut extra = std::collections::HashMap::new();
    insert_encryption_headers(&encryption_headers, &mut extra);

    let params = inference_providers::ImageGenerationParams {
        model: request.model.clone(),
        prompt: request.prompt.clone(),
        n: request.n,
        size: request.size.clone(),
        response_format,
        quality: request.quality.clone(),
        style: request.style.clone(),
        extra,
    };

    // Call the inference provider pool
    match app_state
        .inference_provider_pool
        .image_generation(params, body_hash.hash.clone())
        .await
    {
        Ok(response_with_bytes) => {
            // Store attestation signature for image generation (same pattern as chat completions)
            let attestation_service = app_state.attestation_service.clone();
            let image_id_for_sig = response_with_bytes.response.id.clone();
            tokio::spawn(async move {
                if let Err(e) = attestation_service
                    .store_chat_signature_from_provider(&image_id_for_sig)
                    .await
                {
                    tracing::debug!(error = %e, "Failed to store image generation signature");
                    tracing::warn!("Image generation: signature storage failed");
                } else {
                    tracing::debug!(image_id = %image_id_for_sig, "Stored signature for image generation");
                }
            });

            // Record usage for image generation
            let organization_id = api_key.organization.id.0;
            let workspace_id = api_key.workspace.id.0;
            let api_key_id_str = api_key.api_key.id.0.clone();
            let model_id = model.id;
            let image_count = match i32::try_from(response_with_bytes.response.data.len()) {
                Ok(count) => count,
                Err(_) => {
                    tracing::error!("Too many images in provider response, cannot fit in i32 for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Internal error: too many images in provider response".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };
            let provider_request_id = response_with_bytes.response.id.clone();
            let usage_service = app_state.usage_service.clone();

            // Parse API key ID early for usage recording
            let api_key_id = match Uuid::parse_str(&api_key_id_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                    // Still return response but log error
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(response_with_bytes.raw_bytes))
                        .unwrap();
                }
            };

            let usage_request = build_image_usage_request(
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                &provider_request_id,
                image_count,
                services::usage::InferenceType::ImageGeneration,
            );
            record_usage_with_sync_fallback(usage_service, usage_request, "Image generation").await;

            // Return the exact bytes from the provider for hash verification
            // This ensures clients can hash the response and compare with attestation endpoints
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(response_with_bytes.raw_bytes))
                .unwrap()
        }
        Err(e) => {
            // Log provider errors at debug level to avoid exposing infrastructure details in production
            tracing::debug!(error = %e, "Image generation failed");

            // Map error to appropriate status code and sanitized message
            let (status_code, message) = match &e {
                inference_providers::ImageGenerationError::GenerationError(msg) => {
                    // Check if it's a model not found error
                    if msg.contains("not found") || msg.contains("does not exist") {
                        (StatusCode::NOT_FOUND, "Model not found".to_string())
                    } else {
                        tracing::warn!("Image generation error: service unavailable");
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Image generation failed".to_string(),
                        )
                    }
                }
                inference_providers::ImageGenerationError::HttpError { status_code, .. } => {
                    // Map HTTP status codes appropriately
                    let code = StatusCode::from_u16(*status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    let msg = match code {
                        StatusCode::NOT_FOUND => "Model not found".to_string(),
                        StatusCode::BAD_REQUEST => "Invalid request".to_string(),
                        StatusCode::TOO_MANY_REQUESTS => "Rate limit exceeded".to_string(),
                        _ => {
                            tracing::warn!(
                                http_status = *status_code,
                                "Image generation HTTP error"
                            );
                            "Image generation failed".to_string()
                        }
                    };
                    (code, msg)
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, "server_error".to_string())),
            )
                .into_response()
        }
    }
}

/// Audio transcription endpoint
///
/// Transcribe audio files using Whisper models. Accepts audio file uploads via multipart/form-data.
/// Supports MP3, WAV, WEBM, FLAC, OGG, and M4A formats. Maximum file size: 25 MB.
///
/// **Request Body (multipart/form-data):**
/// All fields should be provided as text values or files as indicated in the schema.
#[utoipa::path(
    post,
    path = "/v1/audio/transcriptions",
    tag = "Audio",
    request_body(content = AudioTranscriptionRequestSchema, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Successful transcription", content(
            (AudioTranscriptionResponse = "application/json"),
            (String = "text/plain")
        )),
        (status = 400, description = "Invalid request (empty file, unsupported format, file too large)", body = ErrorResponse),
        (status = 401, description = "Unauthorized (missing or invalid API key)", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 529, description = "All inference backends overloaded; retry with exponential backoff", body = ErrorResponse),
    ),
    security(("ApiKeyAuth" = []))
)]
pub async fn audio_transcriptions(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    mut multipart: Multipart,
) -> axum::response::Response {
    debug!(
        "Audio transcription request from api key: {:?}",
        api_key.api_key.id
    );

    // Parse multipart form fields
    let mut model: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    let mut language: Option<String> = None;
    let mut response_format: Option<String> = None;
    let mut temperature: Option<f32> = None;
    let mut timestamp_granularities: Option<Vec<String>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let field_name = match field.name() {
            Some(name) => name.to_string(),
            None => {
                tracing::warn!("Multipart field name is missing");
                return (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        "Missing field name in multipart request".to_string(),
                        "invalid_request_error".to_string(),
                    )),
                )
                    .into_response();
            }
        };

        match field_name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                match field.bytes().await {
                    Ok(bytes) => file_bytes = Some(bytes.to_vec()),
                    Err(_) => {
                        // Don't log error details - may contain customer data
                        tracing::error!("Failed to read file field");
                        return (
                            StatusCode::BAD_REQUEST,
                            ResponseJson(ErrorResponse::new(
                                "Failed to read audio file".to_string(),
                                "invalid_request_error".to_string(),
                            )),
                        )
                            .into_response();
                    }
                }
            }
            "model" => {
                if let Ok(value) = field.text().await {
                    model = Some(value);
                }
            }
            "language" => {
                if let Ok(value) = field.text().await {
                    language = Some(value);
                }
            }
            "response_format" => {
                if let Ok(value) = field.text().await {
                    response_format = Some(value);
                }
            }
            "temperature" => {
                if let Ok(value) = field.text().await {
                    if let Ok(temp) = value.parse::<f32>() {
                        temperature = Some(temp);
                    }
                }
            }
            "timestamp_granularities[]" | "timestamp_granularities" => {
                if let Ok(value) = field.text().await {
                    timestamp_granularities
                        .get_or_insert_with(Vec::new)
                        .extend(value.split(',').map(|s| s.trim().to_string()));
                }
            }
            _ => {
                tracing::debug!("Skipping unknown field: {}", field_name);
            }
        }
    }

    // Construct request and validate
    let request = crate::models::AudioTranscriptionRequest {
        model: model.unwrap_or_default(),
        file_bytes: file_bytes.unwrap_or_default(),
        filename: filename.unwrap_or_else(|| "audio.mp3".to_string()),
        language,
        response_format,
        temperature,
        timestamp_granularities,
    };

    debug!(
        "Audio transcription: model={}, filename={}, file_size_kb={}, org={}, workspace={}",
        request.model,
        request.filename,
        request.file_bytes.len() / 1024,
        api_key.organization.id,
        api_key.workspace.id.0
    );

    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                error,
                "invalid_request_error".to_string(),
            )),
        )
            .into_response();
    }

    // Resolve model to get UUID for usage tracking (handles aliases like chat_completions)
    let model = match app_state
        .models_service
        .resolve_and_get_model(&request.model)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", request.model),
                    "not_found_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve model for audio transcription");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    let model_id = model.id;
    let model_name = request.model.clone();
    let organization_id = api_key.organization.id.0;

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    // Convert API request to provider params
    let mut extra = std::collections::HashMap::new();
    insert_encryption_headers(&encryption_headers, &mut extra);

    let requested_response_format = request.response_format.clone();
    let provider_response_format = match requested_response_format.as_deref() {
        // Keep provider parsing stable and preserve duration-based billing by
        // requesting verbose JSON, then returning plain text at the API boundary
        // after usage is recorded.
        Some("text") => Some("verbose_json".to_string()),
        _ => request.response_format.clone(),
    };

    let params = inference_providers::AudioTranscriptionParams {
        model: model_name.clone(),
        file_bytes: request.file_bytes,
        filename: request.filename,
        language: request
            .language
            .map(|language| crate::models::normalize_audio_transcription_language(&language)),
        response_format: provider_response_format,
        temperature: request.temperature,
        timestamp_granularities: request.timestamp_granularities,
        extra,
    };

    // Call completion service which handles concurrent request limiting
    match app_state
        .completion_service
        .audio_transcription(
            organization_id,
            model_id,
            &model_name,
            params,
            body_hash.hash.clone(),
        )
        .await
    {
        Ok(response) => {
            // Record usage for audio transcription SYNCHRONOUSLY before returning response
            // Critical for financial accuracy: if usage recording fails, client gets 500 error
            // rather than 200 success. This prevents lost revenue and maintains audit trail.
            // Bill by audio duration in seconds (use input_tokens field)
            let workspace_id = api_key.workspace.id.0;
            let api_key_id_str = api_key.api_key.id.0.clone();

            // Clamp duration to valid range [0, i32::MAX] to prevent overflow and negative values
            let duration_seconds = response
                .duration
                .unwrap_or(0.0)
                .max(0.0)
                .min(i32::MAX as f64)
                .ceil() as i32;

            // Parse API key ID to UUID
            let api_key_id = match uuid::Uuid::parse_str(&api_key_id_str) {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid API key ID for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to record usage - invalid API key format".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };

            let inference_id = uuid::Uuid::new_v4();
            let usage_request = services::usage::RecordUsageServiceRequest {
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                input_tokens: duration_seconds, // Bill by duration in seconds
                output_tokens: 0,
                cache_read_tokens: 0,
                inference_type: services::usage::ports::InferenceType::AudioTranscription,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(inference_id),
                provider_request_id: None,
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
            };

            // Record usage synchronously - fail the request if usage recording fails
            if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
                tracing::error!(
                    error = %e,
                    %organization_id,
                    %workspace_id,
                    "Failed to record audio transcription usage"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to record usage - please retry".to_string(),
                        "server_error".to_string(),
                    )),
                )
                    .into_response();
            }

            tracing::info!(
                %organization_id,
                %workspace_id,
                duration_seconds,
                "Audio transcription completed and usage recorded successfully"
            );

            if requested_response_format.as_deref() == Some("text") {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    response.text,
                )
                    .into_response()
            } else {
                let response_body = crate::models::AudioTranscriptionResponse::from(response);
                (StatusCode::OK, ResponseJson(response_body)).into_response()
            }
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded(msg) => {
                    tracing::warn!("Concurrent request limit exceeded for audio transcription");
                    (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
                }
                services::completions::ports::CompletionError::ProviderError {
                    status_code,
                    ..
                } => {
                    tracing::error!("Audio transcription provider error");
                    let http_status = StatusCode::from_u16(status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    (
                        http_status,
                        "server_error",
                        "Audio transcription failed. Please try again later.".to_string(),
                    )
                }
                services::completions::ports::CompletionError::ServiceOverloaded(_) => {
                    tracing::warn!("Audio transcription service overloaded");
                    (
                        crate::routes::common::status_overloaded(),
                        "service_overloaded",
                        "All inference backends are overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                _ => {
                    tracing::error!("Unexpected audio transcription error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Audio transcription failed".to_string(),
                    )
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, error_type.to_string())),
            )
                .into_response()
        }
    }
}

/// Edit images from a text prompt and image
///
/// Edit images using an AI model from an image and text description. OpenAI-compatible endpoint.
///
/// **Request Body (multipart/form-data):**
/// All fields should be provided as text values or files as indicated in the schema.
#[utoipa::path(
    post,
    path = "/v1/images/edits",
    tag = "Images",
    request_body(content = ImageEditRequestSchema, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Image edited successfully", body = ImageGenerationResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 413, description = "Payload too large", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 529, description = "All inference backends overloaded; retry with exponential backoff", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn image_edits(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    mut multipart: axum::extract::Multipart,
) -> axum::response::Response {
    debug!("Image edit request from api key: {:?}", api_key.api_key.id);

    let mut model = String::new();
    let mut prompt = String::new();
    let mut image: Option<Vec<u8>> = None;
    let mut size: Option<String> = None;
    let mut response_format: Option<String> = None;

    // Parse multipart form data
    loop {
        match multipart.next_field().await {
            Ok(Some(mut field)) => {
                match field.name().unwrap_or("") {
                    "image" => {
                        // Use streaming validation to detect oversized payloads before buffering entirely in memory.
                        // This prevents memory exhaustion DoS attacks where attackers send payloads larger than MAX_FILE_SIZE.
                        let mut bytes = Vec::new();
                        let mut size = 0usize;

                        loop {
                            match field.chunk().await {
                                Ok(Some(chunk)) => {
                                    size += chunk.len();
                                    // Reject early if size exceeds limit, before allocating memory
                                    if size > MAX_FILE_SIZE {
                                        return (
                                            StatusCode::PAYLOAD_TOO_LARGE,
                                            ResponseJson(ErrorResponse::new(
                                                "Image size exceeds maximum of 512 MB".to_string(),
                                                "invalid_request_error".to_string(),
                                            )),
                                        )
                                            .into_response();
                                    }
                                    bytes.extend_from_slice(&chunk);
                                }
                                Ok(None) => break,
                                Err(e) => {
                                    return (
                                        StatusCode::BAD_REQUEST,
                                        ResponseJson(ErrorResponse::new(
                                            format!("Failed to read image: {}", e),
                                            "invalid_request_error".to_string(),
                                        )),
                                    )
                                        .into_response();
                                }
                            }
                        }

                        image = Some(bytes);
                    }
                    "model" => match field.text().await {
                        Ok(text) => model = text,
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read model: {}", e),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    },
                    "prompt" => match field.text().await {
                        Ok(text) => prompt = text,
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read prompt: {}", e),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    },
                    "size" => match field.text().await {
                        Ok(text) => size = Some(text),
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read size: {}", e),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    },
                    "response_format" => match field.text().await {
                        Ok(text) => response_format = Some(text),
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read response_format: {}", e),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    },
                    _ => {
                        // Ignore unknown fields
                    }
                }
            }
            Ok(None) => break, // All fields read successfully
            Err(e) => {
                // Multipart parsing error (malformed boundary, invalid encoding, etc.)
                let (status, error_message) = analyze_multipart_error(&e);
                tracing::debug!(error = %e, message = %error_message, "Multipart parsing error");
                return (
                    status,
                    ResponseJson(ErrorResponse::new(
                        error_message,
                        "invalid_request_error".to_string(),
                    )),
                )
                    .into_response();
            }
        }
    }

    // Fail fast if image is missing
    let image = match image {
        Some(img) => img,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Missing required field: image".to_string(),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    // Build the request
    let request = crate::models::ImageEditRequest {
        model,
        prompt,
        image,
        size,
        response_format,
    };

    debug!(
        "Image edit request: model={}, org={}, workspace={}",
        request.model, api_key.organization.id, api_key.workspace.id.0
    );

    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                error,
                "invalid_request_error".to_string(),
            )),
        )
            .into_response();
    }

    // Resolve model to get UUID for usage tracking
    let model = match app_state
        .models_service
        .get_model_by_name(&request.model)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", request.model),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::debug!(error = %e, "Failed to resolve model for image edit");
            tracing::warn!("Image edit: model resolution failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    // Validate and enforce response_format for verifiable models
    // Verifiable models (attestation_supported = true) only support "b64_json" format
    // Default to "b64_json" if not specified to prevent downstream server from applying "url" default
    let response_format = if model.attestation_supported {
        match &request.response_format {
            Some(format) if format == "b64_json" => Some(format.clone()),
            Some(format) => {
                return (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!(
                            "response_format '{}' is not supported for verifiable models. Only 'b64_json' is supported.",
                            format
                        ),
                        "invalid_request_error".to_string(),
                    )),
                )
                    .into_response();
            }
            None => Some("b64_json".to_string()), // Default to b64_json for verifiable models
        }
    } else {
        request.response_format.clone()
    };

    // Convert API request to provider params
    let params = inference_providers::ImageEditParams {
        model: request.model.clone(),
        prompt: request.prompt.clone(),
        image: Arc::new(request.image),
        size: request.size,
        response_format,
    };

    // Call the inference provider pool
    match app_state
        .inference_provider_pool
        .image_edit(params, body_hash.hash.clone())
        .await
    {
        Ok(response_with_bytes) => {
            // Store attestation signature for image edit (same pattern as image generation)
            let attestation_service = app_state.attestation_service.clone();
            let image_id_for_sig = response_with_bytes.response.id.clone();
            tokio::spawn(async move {
                if let Err(e) = attestation_service
                    .store_chat_signature_from_provider(&image_id_for_sig)
                    .await
                {
                    tracing::debug!(error = %e, "Failed to store image edit signature");
                    tracing::warn!("Image edit: signature storage failed");
                } else {
                    tracing::debug!(image_id = %image_id_for_sig, "Stored signature for image edit");
                }
            });

            // Record usage for image edit
            let organization_id = api_key.organization.id.0;
            let workspace_id = api_key.workspace.id.0;
            let api_key_id_str = api_key.api_key.id.0.clone();
            let model_id = model.id;
            let image_count = match i32::try_from(response_with_bytes.response.data.len()) {
                Ok(count) => count,
                Err(_) => {
                    tracing::error!("Too many images in provider response, cannot fit in i32 for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Internal error: too many images in provider response".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };
            let provider_request_id = response_with_bytes.response.id.clone();
            let usage_service = app_state.usage_service.clone();

            // Parse API key ID early for usage recording
            let api_key_id = match Uuid::parse_str(&api_key_id_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                    // Still return response but log error
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(response_with_bytes.raw_bytes))
                        .unwrap();
                }
            };

            let usage_request = build_image_usage_request(
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                &provider_request_id,
                image_count,
                services::usage::InferenceType::ImageEdit,
            );
            record_usage_with_sync_fallback(usage_service, usage_request, "Image edit").await;

            // Return the exact bytes from the provider for hash verification
            // This ensures clients can hash the response and compare with attestation endpoints
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(response_with_bytes.raw_bytes))
                .unwrap()
        }
        Err(e) => {
            // Log provider errors at debug level to avoid exposing infrastructure details in production
            tracing::debug!(error = %e, "Image edit failed");

            // Map error to appropriate status code and sanitized message
            let (status_code, message) = match &e {
                inference_providers::ImageEditError::EditError(msg) => {
                    // Check if it's a model not found error
                    if msg.contains("not found") || msg.contains("does not exist") {
                        (StatusCode::NOT_FOUND, "Model not found".to_string())
                    } else {
                        tracing::warn!("Image edit error: service unavailable");
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Image edit failed".to_string(),
                        )
                    }
                }
                inference_providers::ImageEditError::HttpError { status_code, .. } => {
                    // Map HTTP status codes appropriately
                    let code = StatusCode::from_u16(*status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    let msg = match code {
                        StatusCode::NOT_FOUND => "Model not found".to_string(),
                        StatusCode::BAD_REQUEST => "Invalid request".to_string(),
                        StatusCode::TOO_MANY_REQUESTS => "Rate limit exceeded".to_string(),
                        _ => {
                            tracing::warn!(http_status = *status_code, "Image edit HTTP error");
                            "Image edit failed".to_string()
                        }
                    };
                    (code, msg)
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, "server_error".to_string())),
            )
                .into_response()
        }
    }
}

fn apply_rerank_response_options(
    request: &crate::models::RerankRequest,
    response: &mut inference_providers::RerankResponse,
) {
    response
        .results
        .sort_by(|a, b| b.relevance_score.total_cmp(&a.relevance_score));

    if let Some(top_n) = request.top_n {
        response.results.truncate(top_n);
    }

    for result in &mut response.results {
        if request.return_documents == Some(false) {
            result.document = None;
            continue;
        }

        if let Some(serde_json::Value::Object(document)) = result.document.as_mut() {
            if document
                .get("multi_modal")
                .is_some_and(serde_json::Value::is_null)
            {
                document.remove("multi_modal");
            }
        }
    }
}

#[cfg(test)]
mod rerank_response_options_tests {
    use super::*;
    use serde_json::json;

    fn request(
        top_n: Option<usize>,
        return_documents: Option<bool>,
    ) -> crate::models::RerankRequest {
        crate::models::RerankRequest {
            model: "Qwen/Qwen3-Reranker-0.6B".to_string(),
            query: "capital of France".to_string(),
            documents: vec![
                "Paris".to_string(),
                "Berlin".to_string(),
                "Tokyo".to_string(),
            ],
            top_n,
            return_documents,
        }
    }

    fn response() -> inference_providers::RerankResponse {
        inference_providers::RerankResponse {
            id: "rerank-test".to_string(),
            model: "Qwen/Qwen3-Reranker-0.6B".to_string(),
            results: vec![
                inference_providers::RerankResult {
                    index: 2,
                    relevance_score: 0.4,
                    document: Some(json!({"text": "Tokyo", "multi_modal": null})),
                },
                inference_providers::RerankResult {
                    index: 0,
                    relevance_score: 0.9,
                    document: Some(json!({"text": "Paris", "multi_modal": null})),
                },
                inference_providers::RerankResult {
                    index: 1,
                    relevance_score: 0.1,
                    document: Some(json!({"text": "Berlin", "multi_modal": null})),
                },
            ],
            usage: Some(inference_providers::RerankUsage {
                prompt_tokens: Some(10),
                total_tokens: Some(15),
            }),
        }
    }

    #[test]
    fn top_n_sorts_by_relevance_and_truncates_results() {
        let mut response = response();

        apply_rerank_response_options(&request(Some(2), None), &mut response);

        let indices: Vec<i32> = response.results.iter().map(|result| result.index).collect();
        assert_eq!(indices, vec![0, 2]);
    }

    #[test]
    fn return_documents_false_omits_documents() {
        let mut response = response();

        apply_rerank_response_options(&request(None, Some(false)), &mut response);

        assert!(response
            .results
            .iter()
            .all(|result| result.document.is_none()));
    }

    #[test]
    fn null_multi_modal_is_stripped_from_document_objects() {
        let mut response = response();

        apply_rerank_response_options(&request(None, None), &mut response);

        for result in response.results {
            let document = result.document.expect("document should be returned");
            assert!(document.get("text").is_some());
            assert!(document.get("multi_modal").is_none());
        }
    }

    #[test]
    fn top_n_zero_is_invalid() {
        assert_eq!(
            request(Some(0), None).validate().unwrap_err(),
            "top_n must be at least 1"
        );
    }
}

/// Document reranking endpoint
///
/// Ranks documents by relevance to a query using a reranker model.
///
/// **Concurrent Request Limits:** Each organization has a per-model concurrent request limit (default: 64).
/// When the limit is reached, new requests will fail with 429 status code. Wait for in-flight requests to complete before retrying.
#[utoipa::path(
    post,
    path = "/v1/rerank",
    tag = "Rerank",
    request_body = crate::models::RerankRequest,
    responses(
        (status = 200, description = "Successful rerank", body = crate::models::RerankResponse),
        (status = 400, description = "Invalid request (empty documents, invalid model, etc.)", body = ErrorResponse),
        (status = 401, description = "Unauthorized (missing or invalid API key)", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 429, description = "Concurrent request limit exceeded for the organization. Max concurrent requests per model: 64 (configurable)", body = ErrorResponse),
        (status = 500, description = "Server error (billing failure, provider error)", body = ErrorResponse),
        (status = 529, description = "All inference backends overloaded; retry with exponential backoff", body = ErrorResponse),
    ),
    security(("ApiKeyAuth" = []))
)]
pub async fn rerank(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(_body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    OpenAiJson(request): OpenAiJson<crate::models::RerankRequest>,
) -> axum::response::Response {
    debug!(
        "Rerank request: model={}, org={}, workspace={}",
        request.model, api_key.organization.id, api_key.workspace.id.0
    );

    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                error,
                "invalid_request_error".to_string(),
            )),
        )
            .into_response();
    }

    // Resolve model to get UUID for usage tracking - fail fast if not found
    // Models must be registered in the database to be available for use
    let model = match app_state
        .models_service
        .get_model_by_name(&request.model)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", request.model),
                    "not_found_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve model for rerank");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    let model_id = model.id;
    let organization_id = api_key.organization.id.0;

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    // Convert API request to provider params
    let mut extra = std::collections::HashMap::new();
    insert_encryption_headers(&encryption_headers, &mut extra);

    let params = inference_providers::RerankParams {
        model: request.model.clone(),
        query: request.query.clone(),
        documents: request.documents.clone(),
        extra,
    };

    // Call completion service which handles concurrent request limiting
    // Each organization has a per-model concurrent request limit (default: 64 concurrent requests).
    // This prevents resource exhaustion and ensures fair usage. Returns 429 if limit exceeded.
    match app_state
        .completion_service
        .try_rerank(organization_id, model_id, &request.model, params)
        .await
    {
        Ok(mut response) => {
            apply_rerank_response_options(&request, &mut response);

            // Record usage for rerank SYNCHRONOUSLY to ensure billing accuracy
            // This is critical for revenue tracking and must complete before returning response
            let organization_id = api_key.organization.id.0;
            let workspace_id = api_key.workspace.id.0;
            let mut token_count = response
                .usage
                .as_ref()
                .and_then(|u| u.total_tokens)
                .unwrap_or(0);

            // Validate token count is reasonable (prevent provider misreporting)
            const MAX_REASONABLE_TOKENS: i32 = 1_000_000; // 1M tokens max
            let mut token_anomaly_detected = false;

            if token_count > MAX_REASONABLE_TOKENS {
                // Log at ERROR level with full context for monitoring/alerting
                // This indicates a provider bug that needs investigation
                tracing::error!(
                    token_count = token_count,
                    max_expected = MAX_REASONABLE_TOKENS,
                    model = %request.model,
                    organization_id = %organization_id,
                    "Provider returned unreasonable token count - capping to prevent billing errors. This may indicate provider misconfiguration or a bug."
                );
                token_anomaly_detected = true;

                // Record metrics for monitoring and alerting
                let model_tag = format!("model:{}", request.model);
                let reason_tag = format!(
                    "reason:{}",
                    services::metrics::consts::REASON_TOKEN_OVERFLOW
                );
                let anomaly_tags = [model_tag.as_str(), reason_tag.as_str()];
                app_state.metrics_service.record_count(
                    services::metrics::consts::METRIC_PROVIDER_TOKEN_ANOMALIES,
                    1,
                    &anomaly_tags,
                );

                // Cap to maximum to prevent billing errors
                token_count = MAX_REASONABLE_TOKENS;
            }

            // Warn if provider didn't return usage data
            if token_count == 0 {
                tracing::warn!(
                    model = %request.model,
                    organization_id = %organization_id,
                    "Provider returned zero tokens for rerank - no cost will be charged. This may indicate provider misconfiguration or incomplete response."
                );
                token_anomaly_detected = true;

                // Record metrics for monitoring and alerting on missing usage data
                let model_tag = format!("model:{}", request.model);
                let reason_tag =
                    format!("reason:{}", services::metrics::consts::REASON_MISSING_USAGE);
                let zero_tokens_tags = [model_tag.as_str(), reason_tag.as_str()];
                app_state.metrics_service.record_count(
                    services::metrics::consts::METRIC_PROVIDER_ZERO_TOKENS,
                    1,
                    &zero_tokens_tags,
                );
            }

            // If anomaly detected, log additional context for debugging
            if token_anomaly_detected {
                tracing::info!(
                    model = %request.model,
                    organization_id = %organization_id,
                    final_token_count = token_count,
                    "Token count anomaly: Provider data quality issue detected. Recommendation: Check provider logs and configuration."
                );
            }

            // Parse API key ID to UUID
            let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to record usage".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };

            let inference_id = uuid::Uuid::new_v4();
            let usage_request = services::usage::RecordUsageServiceRequest {
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                input_tokens: token_count,
                output_tokens: 0,
                cache_read_tokens: 0,
                inference_type: services::usage::ports::InferenceType::Rerank,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(inference_id),
                provider_request_id: None,
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
            };

            // Record usage synchronously - this is billing-critical and must succeed
            // If usage recording fails, we must fail the request to prevent revenue loss
            if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
                tracing::error!(error = %e, "Failed to record rerank usage - blocking request to ensure billing accuracy");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to record usage - please retry".to_string(),
                        "server_error".to_string(),
                    )),
                )
                    .into_response();
            }

            (StatusCode::OK, ResponseJson(response)).into_response()
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded(msg) => {
                    tracing::warn!("Concurrent request limit exceeded for rerank");
                    (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
                }
                services::completions::ports::CompletionError::ProviderError {
                    status_code,
                    ..
                } => {
                    tracing::error!("Rerank provider error");
                    let http_status = StatusCode::from_u16(status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    (
                        http_status,
                        "server_error",
                        "Reranking failed. Please try again later.".to_string(),
                    )
                }
                services::completions::ports::CompletionError::InvalidModel(msg) => {
                    tracing::warn!("Rerank model not found");
                    (StatusCode::NOT_FOUND, "not_found_error", msg)
                }
                services::completions::ports::CompletionError::ServiceOverloaded(_) => {
                    tracing::warn!("Rerank service overloaded");
                    (
                        crate::routes::common::status_overloaded(),
                        "service_overloaded",
                        "All inference backends are overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                _ => {
                    tracing::error!("Unexpected rerank error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Reranking failed".to_string(),
                    )
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, error_type.to_string())),
            )
                .into_response()
        }
    }
}

/// Text embeddings endpoint (passthrough)
///
/// Proxies embedding requests to the appropriate backend model.
/// Only the `model` field is read from the request for routing; the rest is forwarded as-is.
///
/// These documentation-only types approximate the request and response bodies
/// for OpenAPI schema generation. The runtime handler still accepts a raw
/// `Bytes` body and forwards it transparently to the provider.
#[derive(utoipa::ToSchema, serde::Serialize, serde::Deserialize)]
struct EmbeddingsRequestDoc {
    /// ID of the model to use for generating embeddings.
    model: String,
    /// Input text or tokens to embed. This may be a string, array of strings,
    /// or other JSON-compatible structure, so we document it generically.
    #[serde(default)]
    input: serde_json::Value,
}

#[derive(utoipa::ToSchema, serde::Serialize, serde::Deserialize)]
struct EmbeddingsResponseDoc {
    /// Provider-specific embeddings payload; documented generically.
    #[serde(default)]
    data: serde_json::Value,
}

/// Classify an upstream provider HTTP error into the (status, error_type, message)
/// triple we surface to the client.
///
/// Rules:
/// - 401/403/407 from upstream mean *our* credentials to the backend are wrong
///   (or our backend's auth config is broken). The client did nothing to cause
///   it, so we return 500 with a generic message instead of echoing an auth
///   failure that would be misleading.
/// - 404 → 404 not_found_error (e.g. model not found at this provider).
/// - 429 → 429 rate_limit_error.
/// - Other 4xx → preserve upstream status with `invalid_request_error`. The
///   upstream message is surfaced so the user can see *why* their request was
///   rejected (e.g. "dimensions is not supported for this model").
/// - 5xx (and anything else) → 500 server_error with a generic message. The
///   upstream body may contain stack traces or internal details we don't want
///   to leak.
fn classify_provider_error(
    upstream_status: u16,
    upstream_message: String,
) -> (StatusCode, &'static str, String) {
    let generic = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Embeddings request failed. Please try again later.".to_string(),
        )
    };
    match upstream_status {
        401 | 403 | 407 => generic(),
        404 => (StatusCode::NOT_FOUND, "not_found_error", upstream_message),
        429 => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            upstream_message,
        ),
        s if (400..=499).contains(&s) => {
            let http_status = StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_REQUEST);
            (http_status, "invalid_request_error", upstream_message)
        }
        _ => generic(),
    }
}

#[utoipa::path(
    post,
    path = "/v1/embeddings",
    tag = "Embeddings",
    request_body = EmbeddingsRequestDoc,
    responses(
        (status = 200, description = "Embeddings generated successfully", body = EmbeddingsResponseDoc),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 529, description = "All inference backends overloaded; retry with exponential backoff", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn embeddings(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(_body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    // Minimal deserialization: extract only the model name for routing
    #[derive(serde::Deserialize)]
    struct ModelExtract {
        model: String,
    }

    let model_name = match serde_json::from_slice::<ModelExtract>(&body) {
        Ok(extract) => extract.model,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    format!("Invalid request body: {e}"),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    debug!(
        "Embeddings request: model={}, org={}, workspace={}",
        model_name, api_key.organization.id, api_key.workspace.id.0
    );

    // Resolve model to get UUID for usage tracking
    let model = match app_state
        .models_service
        .get_model_by_name(&model_name)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", model_name),
                    "not_found_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve model for embeddings");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    let model_id = model.id;
    let organization_id = api_key.organization.id.0;

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };
    let mut extra = std::collections::HashMap::new();
    insert_encryption_headers(&encryption_headers, &mut extra);

    match app_state
        .completion_service
        .try_embeddings(organization_id, model_id, &model_name, body, extra)
        .await
    {
        Ok(response_bytes) => {
            // Minimal deserialization: extract only usage for billing
            #[derive(serde::Deserialize)]
            struct UsageExtract {
                usage: Option<UsageFields>,
            }
            #[derive(serde::Deserialize)]
            struct UsageFields {
                prompt_tokens: Option<i32>,
                total_tokens: Option<i32>,
            }

            let mut token_count = serde_json::from_slice::<UsageExtract>(&response_bytes)
                .ok()
                .and_then(|u| u.usage)
                .and_then(|u| u.total_tokens.or(u.prompt_tokens))
                .unwrap_or(0);

            // Token anomaly detection
            const MAX_REASONABLE_TOKENS: i32 = 1_000_000;
            let mut token_anomaly_detected = false;

            if token_count > MAX_REASONABLE_TOKENS {
                tracing::error!(
                    token_count = token_count,
                    max_expected = MAX_REASONABLE_TOKENS,
                    model = %model_name,
                    organization_id = %organization_id,
                    "Provider returned unreasonable token count for embeddings - capping"
                );
                token_anomaly_detected = true;
                let model_tag = format!("model:{}", model_name);
                let reason_tag = format!(
                    "reason:{}",
                    services::metrics::consts::REASON_TOKEN_OVERFLOW
                );
                let anomaly_tags = [model_tag.as_str(), reason_tag.as_str()];
                app_state.metrics_service.record_count(
                    services::metrics::consts::METRIC_PROVIDER_TOKEN_ANOMALIES,
                    1,
                    &anomaly_tags,
                );
                token_count = MAX_REASONABLE_TOKENS;
            }

            if token_count == 0 {
                tracing::warn!(
                    model = %model_name,
                    organization_id = %organization_id,
                    "Provider returned zero tokens for embeddings"
                );
                token_anomaly_detected = true;
                let model_tag = format!("model:{}", model_name);
                let reason_tag =
                    format!("reason:{}", services::metrics::consts::REASON_MISSING_USAGE);
                let zero_tokens_tags = [model_tag.as_str(), reason_tag.as_str()];
                app_state.metrics_service.record_count(
                    services::metrics::consts::METRIC_PROVIDER_ZERO_TOKENS,
                    1,
                    &zero_tokens_tags,
                );
            }

            if token_anomaly_detected {
                tracing::info!(
                    model = %model_name,
                    organization_id = %organization_id,
                    final_token_count = token_count,
                    "Token count anomaly: Provider data quality issue detected. Recommendation: Check provider logs and configuration."
                );
            }

            // Record usage synchronously
            let workspace_id = api_key.workspace.id.0;
            let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to record usage".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };

            let inference_id = uuid::Uuid::new_v4();
            let usage_request = services::usage::RecordUsageServiceRequest {
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                input_tokens: token_count,
                output_tokens: 0,
                cache_read_tokens: 0,
                inference_type: services::usage::ports::InferenceType::Embedding,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(inference_id),
                provider_request_id: None,
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
            };

            if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
                tracing::error!(error = %e, "Failed to record embeddings usage");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to record usage - please retry".to_string(),
                        "server_error".to_string(),
                    )),
                )
                    .into_response();
            }

            // Return raw response bytes
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(response_bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded(msg) => {
                    tracing::warn!("Concurrent request limit exceeded for embeddings");
                    (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
                }
                services::completions::ports::CompletionError::ProviderError {
                    status_code,
                    message,
                } => {
                    let classified = classify_provider_error(status_code, message);
                    if classified.0.is_client_error() {
                        tracing::warn!(
                            upstream_status = status_code,
                            "Embeddings rejected by upstream with client error"
                        );
                    } else {
                        tracing::error!(upstream_status = status_code, "Embeddings provider error");
                    }
                    classified
                }
                services::completions::ports::CompletionError::InvalidModel(msg) => {
                    tracing::warn!("Embeddings model not found");
                    (StatusCode::NOT_FOUND, "not_found_error", msg)
                }
                services::completions::ports::CompletionError::ServiceOverloaded(_) => {
                    tracing::warn!("Embeddings service overloaded");
                    (
                        crate::routes::common::status_overloaded(),
                        "service_overloaded",
                        "All inference backends are overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                _ => {
                    tracing::error!("Unexpected embeddings error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Embeddings request failed".to_string(),
                    )
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, error_type.to_string())),
            )
                .into_response()
        }
    }
}

/// Privacy classification endpoint (passthrough)
///
/// Proxies privacy classification requests to a token-classification model
/// (e.g. `openai/privacy-filter`) that returns PII spans with categories and scores.
/// Only the `model` field is read from the request for routing; the rest is forwarded as-is.
///
/// These documentation-only types approximate the request and response bodies
/// for OpenAPI schema generation. The runtime handler still accepts a raw
/// `Bytes` body and forwards it transparently to the provider.
#[derive(utoipa::ToSchema, serde::Serialize, serde::Deserialize)]
struct PrivacyClassifyRequestDoc {
    /// ID of the model to use for privacy classification.
    model: String,
    /// Text or list of texts to classify. Either a string or array of strings.
    #[serde(default)]
    input: serde_json::Value,
    /// Optional minimum confidence score for returned spans (0.0–1.0).
    #[serde(default)]
    threshold: Option<f64>,
}

#[derive(utoipa::ToSchema, serde::Serialize, serde::Deserialize)]
struct PrivacyClassifyResponseDoc {
    /// Model identifier that produced the classification.
    #[serde(default)]
    model: String,
    /// Per-input classification results; each entry contains `spans` and per-input `usage`.
    #[serde(default)]
    data: serde_json::Value,
}

#[utoipa::path(
    post,
    path = "/v1/privacy/classify",
    tag = "Privacy",
    request_body = PrivacyClassifyRequestDoc,
    responses(
        (status = 200, description = "Privacy classification completed successfully", body = PrivacyClassifyResponseDoc),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded", body = ErrorResponse),
        (status = 529, description = "All inference backends overloaded; retry with exponential backoff", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn privacy_classify(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(_body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    // Minimal deserialization: extract only the model name for routing
    #[derive(serde::Deserialize)]
    struct ModelExtract {
        model: String,
    }

    let model_name = match serde_json::from_slice::<ModelExtract>(&body) {
        Ok(extract) => extract.model,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    format!("Invalid request body: {e}"),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    debug!(
        "Privacy classify request: model={}, org={}, workspace={}",
        model_name, api_key.organization.id, api_key.workspace.id.0
    );

    let model = match app_state
        .models_service
        .get_model_by_name(&model_name)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", model_name),
                    "not_found_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve model for privacy classify");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    let model_id = model.id;
    let organization_id = api_key.organization.id.0;

    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };
    let mut extra = std::collections::HashMap::new();
    insert_encryption_headers(&encryption_headers, &mut extra);

    match app_state
        .completion_service
        .try_privacy_classify(organization_id, model_id, &model_name, body, extra)
        .await
    {
        Ok(response_bytes) => {
            // Privacy filter response shape:
            //   { "model": "...", "data": [{ "index": i, "spans": [...], "usage": { "input_tokens": N } }, ...] }
            // Sum per-item input_tokens for billing.
            #[derive(serde::Deserialize)]
            struct UsageExtract {
                #[serde(default)]
                data: Vec<DataEntry>,
            }
            #[derive(serde::Deserialize)]
            struct DataEntry {
                #[serde(default)]
                usage: Option<UsageFields>,
            }
            #[derive(serde::Deserialize)]
            struct UsageFields {
                input_tokens: Option<i32>,
            }

            // Sum into i64 with saturating arithmetic and clamp to i32. The provider
            // controls both the number of data entries and per-entry input_tokens, so a
            // naive `sum::<i32>()` could wrap silently in release builds before the
            // MAX_REASONABLE_TOKENS check, recording negative usage.
            let token_sum_i64: i64 = serde_json::from_slice::<UsageExtract>(&response_bytes)
                .ok()
                .map(|u| {
                    u.data
                        .into_iter()
                        .filter_map(|d| d.usage.and_then(|x| x.input_tokens))
                        .filter(|t| *t >= 0)
                        .fold(0i64, |acc, t| acc.saturating_add(t as i64))
                })
                .unwrap_or(0);
            let mut token_count = token_sum_i64.clamp(0, i32::MAX as i64) as i32;

            const MAX_REASONABLE_TOKENS: i32 = 1_000_000;
            let mut token_anomaly_detected = false;

            if token_count > MAX_REASONABLE_TOKENS {
                tracing::error!(
                    token_count = token_count,
                    max_expected = MAX_REASONABLE_TOKENS,
                    model = %model_name,
                    organization_id = %organization_id,
                    "Provider returned unreasonable token count for privacy classify - capping"
                );
                token_anomaly_detected = true;
                let model_tag = format!("model:{}", model_name);
                let reason_tag = format!(
                    "reason:{}",
                    services::metrics::consts::REASON_TOKEN_OVERFLOW
                );
                let anomaly_tags = [model_tag.as_str(), reason_tag.as_str()];
                app_state.metrics_service.record_count(
                    services::metrics::consts::METRIC_PROVIDER_TOKEN_ANOMALIES,
                    1,
                    &anomaly_tags,
                );
                token_count = MAX_REASONABLE_TOKENS;
            }

            if token_count == 0 {
                tracing::warn!(
                    model = %model_name,
                    organization_id = %organization_id,
                    "Provider returned zero tokens for privacy classify"
                );
                token_anomaly_detected = true;
                let model_tag = format!("model:{}", model_name);
                let reason_tag =
                    format!("reason:{}", services::metrics::consts::REASON_MISSING_USAGE);
                let zero_tokens_tags = [model_tag.as_str(), reason_tag.as_str()];
                app_state.metrics_service.record_count(
                    services::metrics::consts::METRIC_PROVIDER_ZERO_TOKENS,
                    1,
                    &zero_tokens_tags,
                );
            }

            if token_anomaly_detected {
                tracing::info!(
                    model = %model_name,
                    organization_id = %organization_id,
                    final_token_count = token_count,
                    "Token count anomaly: Provider data quality issue detected. Recommendation: Check provider logs and configuration."
                );
            }

            let workspace_id = api_key.workspace.id.0;
            let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to record usage".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };

            let inference_id = uuid::Uuid::new_v4();
            let usage_request = services::usage::RecordUsageServiceRequest {
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                input_tokens: token_count,
                output_tokens: 0,
                cache_read_tokens: 0,
                inference_type: services::usage::ports::InferenceType::PrivacyClassify,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(inference_id),
                provider_request_id: None,
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
            };

            if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
                tracing::error!(error = %e, "Failed to record privacy classify usage");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to record usage - please retry".to_string(),
                        "server_error".to_string(),
                    )),
                )
                    .into_response();
            }

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(response_bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded(msg) => {
                    tracing::warn!("Concurrent request limit exceeded for privacy classify");
                    (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
                }
                services::completions::ports::CompletionError::ProviderError {
                    status_code,
                    ..
                } => {
                    tracing::error!("Privacy classify provider error");
                    let http_status = StatusCode::from_u16(status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    (
                        http_status,
                        "server_error",
                        "Privacy classify request failed. Please try again later.".to_string(),
                    )
                }
                services::completions::ports::CompletionError::InvalidModel(msg) => {
                    tracing::warn!("Privacy classify model not found");
                    (StatusCode::NOT_FOUND, "not_found_error", msg)
                }
                services::completions::ports::CompletionError::ServiceOverloaded(_) => {
                    tracing::warn!("Privacy classify service overloaded");
                    (
                        crate::routes::common::status_overloaded(),
                        "service_overloaded",
                        "All inference backends are overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                _ => {
                    tracing::error!("Unexpected privacy classify error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Privacy classify request failed".to_string(),
                    )
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, error_type.to_string())),
            )
                .into_response()
        }
    }
}

/// Privacy redaction endpoint
///
/// Sends each input text through the privacy-filter model and returns it
/// rewritten with realistic-looking dummies in place of detected PII
/// (e.g. `redacted1@example.com`, `+1-555-0100`, `Redacted001`). Useful as
/// a one-shot sanitizer ahead of calling a third-party LLM. The original
/// PII is **never** echoed back; only the redacted form is returned.
///
/// Internally this is a `/v1/privacy/classify` call plus local span-apply
/// — same billing (one input_tokens charge per input), same concurrency
/// limits, same model routing.
#[derive(utoipa::ToSchema, serde::Serialize, serde::Deserialize)]
struct PrivacyRedactRequestDoc {
    /// ID of the model to use for redaction (typically `openai/privacy-filter`).
    model: String,
    /// Text or list of texts to redact. Either a string or array of strings.
    #[serde(default)]
    input: serde_json::Value,
    /// Optional minimum confidence score for redacted spans (0.0–1.0).
    #[serde(default)]
    threshold: Option<f64>,
}

#[derive(utoipa::ToSchema, serde::Serialize, serde::Deserialize)]
struct PrivacyRedactDataEntryDoc {
    /// Index of this entry into the input list (matches input order).
    index: usize,
    /// Input text rewritten with placeholders in place of detected PII.
    redacted: String,
    /// Per-input usage (input_tokens billed for this entry).
    #[serde(default)]
    usage: serde_json::Value,
}

#[derive(utoipa::ToSchema, serde::Serialize, serde::Deserialize)]
struct PrivacyRedactResponseDoc {
    /// Model identifier that produced the redaction.
    #[serde(default)]
    model: String,
    /// Per-input redaction results.
    data: Vec<PrivacyRedactDataEntryDoc>,
}

#[utoipa::path(
    post,
    path = "/v1/privacy/redact",
    tag = "Privacy",
    request_body = PrivacyRedactRequestDoc,
    responses(
        (status = 200, description = "Privacy redaction completed successfully", body = PrivacyRedactResponseDoc),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn privacy_redact(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(_body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    #[derive(serde::Deserialize)]
    struct RedactRequest {
        model: String,
        #[serde(default)]
        input: serde_json::Value,
        #[serde(default)]
        threshold: Option<f64>,
    }

    let parsed = match serde_json::from_slice::<RedactRequest>(&body) {
        Ok(req) => req,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    format!("Invalid request body: {e}"),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    // Normalize input into Vec<String>. Accept either a single string or
    // an array of strings — matches the privacy-filter request shape and
    // the classify endpoint's behavior.
    let texts: Vec<String> = match &parsed.input {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return (
                            StatusCode::BAD_REQUEST,
                            ResponseJson(ErrorResponse::new(
                                format!("input[{i}] must be a string"),
                                "invalid_request_error".to_string(),
                            )),
                        )
                            .into_response();
                    }
                }
            }
            out
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "input must be a string or array of strings".to_string(),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    if texts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "input must not be empty".to_string(),
                "invalid_request_error".to_string(),
            )),
        )
            .into_response();
    }

    if let Some(t) = parsed.threshold {
        // Boundary validation: range-check before forwarding upstream so a
        // bad value surfaces as a 400 here, not a confusing 5xx from the
        // model. NaN must be rejected explicitly — `0.0..=1.0` would let
        // it slip through because all comparisons against NaN are false
        // (so `!contains` is false too).
        if t.is_nan() || !(0.0..=1.0).contains(&t) {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "threshold must be a number in [0.0, 1.0]".to_string(),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
    }

    let model_name = parsed.model;

    debug!(
        "Privacy redact request: model={}, org={}, workspace={}, n_inputs={}",
        model_name,
        api_key.organization.id,
        api_key.workspace.id.0,
        texts.len()
    );

    let model = match app_state
        .models_service
        .get_model_by_name(&model_name)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", model_name),
                    "not_found_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve model for privacy redact");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    let model_id = model.id;
    let organization_id = api_key.organization.id.0;

    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };
    let mut extra = std::collections::HashMap::new();
    insert_encryption_headers(&encryption_headers, &mut extra);

    // Forward a normalized classify request to the upstream model. We
    // deliberately rebuild the body rather than passing the client body
    // through: the redact endpoint accepts the same wire shape as classify,
    // but we want a single canonical form (array input) for the upstream
    // call so downstream parsing always yields one data entry per text.
    //
    // Threshold passes through verbatim — only include it if the client
    // sent one. Injecting a default here would diverge from /privacy/classify
    // (which is a pure passthrough and lets the model pick its own default).
    let mut upstream_body = serde_json::json!({
        "model": &model_name,
        "input": &texts,
    });
    if let Some(t) = parsed.threshold {
        upstream_body["threshold"] = serde_json::json!(t);
    }
    let upstream_bytes = match serde_json::to_vec(&upstream_body) {
        Ok(b) => bytes::Bytes::from(b),
        Err(e) => {
            tracing::error!(error = %e, "Failed to encode upstream redact body");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to encode upstream request".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    let response_bytes = match app_state
        .completion_service
        .try_privacy_classify(
            organization_id,
            model_id,
            &model_name,
            upstream_bytes,
            extra,
        )
        .await
    {
        Ok(b) => b,
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded(msg) => {
                    tracing::warn!("Concurrent request limit exceeded for privacy redact");
                    (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
                }
                services::completions::ports::CompletionError::ProviderError {
                    status_code,
                    ..
                } => {
                    tracing::error!("Privacy redact provider error");
                    let http_status = StatusCode::from_u16(status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    (
                        http_status,
                        "server_error",
                        "Privacy redact request failed. Please try again later.".to_string(),
                    )
                }
                services::completions::ports::CompletionError::InvalidModel(msg) => {
                    tracing::warn!("Privacy redact model not found");
                    (StatusCode::NOT_FOUND, "not_found_error", msg)
                }
                services::completions::ports::CompletionError::ServiceOverloaded(_) => {
                    tracing::warn!("Privacy redact service overloaded");
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "service_overloaded",
                        "The service is temporarily overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                _ => {
                    tracing::error!("Unexpected privacy redact error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Privacy redact request failed".to_string(),
                    )
                }
            };
            return (
                status_code,
                ResponseJson(ErrorResponse::new(message, error_type.to_string())),
            )
                .into_response();
        }
    };

    // Apply spans locally. `apply_detected_spans` only does in-process work
    // (JSON parse + UTF-8 boundary checks), so only `Internal` can fire here
    // — handle every variant uniformly as a 500.
    let redacted = match services::auto_redact::apply_detected_spans(&texts, &response_bytes) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Privacy redact apply failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to apply redactions".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    // Pull per-entry usage from the upstream response (same shape as
    // classify) so we can both bill the org and surface per-entry
    // input_tokens to the client.
    #[derive(serde::Deserialize)]
    struct UsageExtract {
        #[serde(default)]
        data: Vec<DataEntry>,
    }
    #[derive(serde::Deserialize)]
    struct DataEntry {
        #[serde(default)]
        index: usize,
        #[serde(default)]
        usage: Option<UsageFields>,
    }
    #[derive(serde::Deserialize, Clone, Copy)]
    struct UsageFields {
        input_tokens: Option<i32>,
    }

    let parsed_usage: UsageExtract =
        serde_json::from_slice(&response_bytes).unwrap_or(UsageExtract { data: Vec::new() });

    // Dedupe by index *before* summing. A buggy provider that returns two
    // entries with the same `index` would otherwise be billed twice while
    // the client only sees the last entry's `usage` in the response —
    // that's a billing/UI inconsistency. Last-write-wins keeps the bill
    // and the surfaced per-entry usage in sync.
    let mut per_index_usage: std::collections::HashMap<usize, UsageFields> =
        std::collections::HashMap::new();
    for d in &parsed_usage.data {
        if d.index >= texts.len() {
            continue;
        }
        if let Some(u) = d.usage {
            per_index_usage.insert(d.index, u);
        }
    }
    let token_sum_i64: i64 = per_index_usage
        .values()
        .filter_map(|u| u.input_tokens)
        .filter(|t| *t >= 0)
        .fold(0i64, |acc, t| acc.saturating_add(t as i64));
    let mut token_count = token_sum_i64.clamp(0, i32::MAX as i64) as i32;

    const MAX_REASONABLE_TOKENS: i32 = 1_000_000;
    let mut token_anomaly_detected = false;
    if token_count > MAX_REASONABLE_TOKENS {
        tracing::error!(
            token_count = token_count,
            max_expected = MAX_REASONABLE_TOKENS,
            model = %model_name,
            organization_id = %organization_id,
            "Provider returned unreasonable token count for privacy redact - capping"
        );
        token_anomaly_detected = true;
        let model_tag = format!("model:{}", model_name);
        let reason_tag = format!(
            "reason:{}",
            services::metrics::consts::REASON_TOKEN_OVERFLOW
        );
        let anomaly_tags = [model_tag.as_str(), reason_tag.as_str()];
        app_state.metrics_service.record_count(
            services::metrics::consts::METRIC_PROVIDER_TOKEN_ANOMALIES,
            1,
            &anomaly_tags,
        );
        token_count = MAX_REASONABLE_TOKENS;
    }
    if token_count == 0 {
        tracing::warn!(
            model = %model_name,
            organization_id = %organization_id,
            "Provider returned zero tokens for privacy redact"
        );
        token_anomaly_detected = true;
        let model_tag = format!("model:{}", model_name);
        let reason_tag = format!("reason:{}", services::metrics::consts::REASON_MISSING_USAGE);
        let zero_tokens_tags = [model_tag.as_str(), reason_tag.as_str()];
        app_state.metrics_service.record_count(
            services::metrics::consts::METRIC_PROVIDER_ZERO_TOKENS,
            1,
            &zero_tokens_tags,
        );
    }
    if token_anomaly_detected {
        tracing::info!(
            model = %model_name,
            organization_id = %organization_id,
            final_token_count = token_count,
            "Token count anomaly: Provider data quality issue detected."
        );
    }

    let workspace_id = api_key.workspace.id.0;
    let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "Invalid API key ID for usage tracking");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to record usage".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    let inference_id = uuid::Uuid::new_v4();
    let usage_request = services::usage::RecordUsageServiceRequest {
        organization_id,
        workspace_id,
        api_key_id,
        model_id,
        input_tokens: token_count,
        output_tokens: 0,
        cache_read_tokens: 0,
        // Redact reuses the privacy-filter classifier under the hood, so
        // bill it the same way. If we ever want analytics to distinguish
        // redact vs classify, add a separate InferenceType variant and a
        // DB-schema-compatible migration.
        inference_type: services::usage::ports::InferenceType::PrivacyClassify,
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(inference_id),
        provider_request_id: None,
        stop_reason: Some(services::usage::StopReason::Completed),
        response_id: None,
        image_count: None,
    };

    if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
        tracing::error!(error = %e, "Failed to record privacy redact usage");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                "Failed to record usage - please retry".to_string(),
                "server_error".to_string(),
            )),
        )
            .into_response();
    }

    let data: Vec<serde_json::Value> = redacted
        .into_iter()
        .enumerate()
        .map(|(idx, redacted_text)| {
            let usage_json = per_index_usage
                .get(&idx)
                .and_then(|u| u.input_tokens)
                .map(|t| serde_json::json!({ "input_tokens": t }))
                .unwrap_or_else(|| serde_json::json!({}));
            serde_json::json!({
                "index": idx,
                "redacted": redacted_text,
                "usage": usage_json,
            })
        })
        .collect();

    let body = serde_json::json!({
        "model": model_name,
        "data": data,
    });
    let body_bytes = match serde_json::to_vec(&body) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "Failed to encode privacy redact response");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to encode response".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body_bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Text similarity scoring endpoint
///
/// Scores the similarity between two texts using a scoring/ranking model.
///
/// **Concurrent Request Limits:** Each organization has a per-model concurrent request limit (default: 64).
/// When the limit is reached, new requests will fail with 429 status code. Wait for in-flight requests to complete before retrying.
#[utoipa::path(
    post,
    path = "/v1/score",
    tag = "Score",
    request_body = crate::models::ScoreRequest,
    responses(
        (status = 200, description = "Successful score", body = crate::models::ScoreResponse),
        (status = 400, description = "Invalid request (empty texts, invalid model, etc.)", body = ErrorResponse),
        (status = 401, description = "Unauthorized (missing or invalid API key)", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 429, description = "Concurrent request limit exceeded for the organization. Max concurrent requests per model: 64 (configurable)", body = ErrorResponse),
        (status = 500, description = "Server error (billing failure, provider error)", body = ErrorResponse),
        (status = 529, description = "All inference backends overloaded; retry with exponential backoff", body = ErrorResponse),
    ),
    security(("ApiKeyAuth" = []))
)]
pub async fn score(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    OpenAiJson(request): OpenAiJson<crate::models::ScoreRequest>,
) -> axum::response::Response {
    debug!(
        "Score request: model={}, org={}, workspace={}",
        request.model, api_key.organization.id, api_key.workspace.id.0
    );

    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                error,
                "invalid_request_error".to_string(),
            )),
        )
            .into_response();
    }

    // Resolve model to get UUID for usage tracking - fail fast if not found
    // Models must be registered in the database to be available for use
    let model = match app_state
        .models_service
        .get_model_by_name(&request.model)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", request.model),
                    "not_found_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve model for score");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    let model_id = model.id;
    let organization_id = api_key.organization.id.0;

    // Call completion service which handles concurrent request limiting
    // Each organization has a per-model concurrent request limit (default: 64 concurrent requests).
    // This prevents resource exhaustion and ensures fair usage. Returns 429 if limit exceeded.
    match app_state
        .completion_service
        .try_score(
            organization_id,
            model_id,
            &request.model,
            body_hash.hash.clone(),
            {
                // Extract and validate encryption headers if present
                let encryption_headers =
                    match crate::routes::common::validate_encryption_headers(&headers) {
                        Ok(headers) => headers,
                        Err(err) => return err.into_response(),
                    };
                let mut extra = std::collections::HashMap::new();
                insert_encryption_headers(&encryption_headers, &mut extra);

                inference_providers::ScoreParams {
                    model: request.model.clone(),
                    text_1: request.text_1.clone(),
                    text_2: request.text_2.clone(),
                    extra,
                }
            },
        )
        .await
    {
        Ok(response) => {
            // Record usage for score SYNCHRONOUSLY to ensure billing accuracy
            // This is critical for revenue tracking and must complete before returning response
            let organization_id = api_key.organization.id.0;
            let workspace_id = api_key.workspace.id.0;

            // Parse API key ID to UUID
            let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to record usage".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };

            let inference_id = hash_inference_id_to_uuid(&response.id);

            // Score requests don't have traditional token counts - use input token count as 1
            let usage_request = services::usage::ports::RecordUsageServiceRequest {
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                input_tokens: 1,
                output_tokens: 0,
                cache_read_tokens: 0,
                inference_type: services::usage::ports::InferenceType::Score,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(inference_id),
                provider_request_id: None,
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
            };

            // Record usage with timeout to prevent blocking responses
            match tokio::time::timeout(
                Duration::from_secs(USAGE_RECORDING_TIMEOUT_SECS),
                app_state.usage_service.record_usage(usage_request),
            )
            .await
            {
                Ok(Ok(_)) => ResponseJson(response).into_response(),
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        model_id = %model_id,
                        "Failed to record usage synchronously, retrying async"
                    );
                    let usage_service_clone = app_state.usage_service.clone();
                    let usage_request_retry = services::usage::ports::RecordUsageServiceRequest {
                        organization_id,
                        workspace_id,
                        api_key_id,
                        model_id,
                        input_tokens: 1,
                        output_tokens: 0,
                        cache_read_tokens: 0,
                        inference_type: services::usage::ports::InferenceType::Score,
                        ttft_ms: None,
                        avg_itl_ms: None,
                        inference_id: Some(inference_id),
                        provider_request_id: None,
                        stop_reason: Some(services::usage::StopReason::Completed),
                        response_id: None,
                        image_count: None,
                    };
                    tokio::spawn(async move {
                        if let Err(e) = usage_service_clone.record_usage(usage_request_retry).await
                        {
                            tracing::error!(
                                error = %e,
                                model_id = %model_id,
                                "Failed to record score usage in async retry"
                            );
                        }
                    });
                    ResponseJson(response).into_response()
                }
                Err(_timeout) => {
                    // Recording timed out, fall back to async retry to avoid blocking client
                    tracing::warn!(
                        model_id = %model_id,
                        "Score usage recording timed out, retrying async"
                    );
                    let usage_service_clone = app_state.usage_service.clone();
                    let usage_request_retry = services::usage::ports::RecordUsageServiceRequest {
                        organization_id,
                        workspace_id,
                        api_key_id,
                        model_id,
                        input_tokens: 1,
                        output_tokens: 0,
                        cache_read_tokens: 0,
                        inference_type: services::usage::ports::InferenceType::Score,
                        ttft_ms: None,
                        avg_itl_ms: None,
                        inference_id: Some(inference_id),
                        provider_request_id: None,
                        stop_reason: Some(services::usage::StopReason::Completed),
                        response_id: None,
                        image_count: None,
                    };
                    tokio::spawn(async move {
                        if let Err(e) = usage_service_clone.record_usage(usage_request_retry).await
                        {
                            tracing::error!(
                                error = %e,
                                model_id = %model_id,
                                "Failed to record score usage in async retry"
                            );
                        }
                    });
                    ResponseJson(response).into_response()
                }
            }
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded(msg) => {
                    tracing::warn!("Concurrent request limit exceeded for score");
                    (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
                }
                services::completions::ports::CompletionError::ProviderError {
                    status_code,
                    ..
                } => {
                    tracing::error!("Score provider error");
                    let http_status = StatusCode::from_u16(status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    (
                        http_status,
                        "server_error",
                        "Scoring failed. Please try again later.".to_string(),
                    )
                }
                services::completions::ports::CompletionError::InvalidModel(msg) => {
                    tracing::warn!("Score model not found");
                    (StatusCode::NOT_FOUND, "not_found_error", msg)
                }
                services::completions::ports::CompletionError::ServiceOverloaded(_) => {
                    tracing::warn!("Score service overloaded");
                    (
                        crate::routes::common::status_overloaded(),
                        "service_overloaded",
                        "All inference backends are overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                _ => {
                    tracing::error!("Unexpected score error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Scoring failed".to_string(),
                    )
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, error_type.to_string())),
            )
                .into_response()
        }
    }
}
