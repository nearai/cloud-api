use super::*;

/// Which privacy endpoint an error response is built for.
#[derive(Clone, Copy, Debug)]
enum PrivacyEndpoint {
    Classify,
    Redact,
}

impl PrivacyEndpoint {
    fn label(self) -> &'static str {
        match self {
            Self::Classify => "classify",
            Self::Redact => "redact",
        }
    }
}

/// Map a `try_privacy_classify` failure to the client-facing
/// `(status, error.type, error.message)` for `endpoint`.
fn privacy_error_parts(
    endpoint: PrivacyEndpoint,
    e: services::completions::ports::CompletionError,
) -> (StatusCode, &'static str, String) {
    use services::completions::ports::CompletionError;
    let label = endpoint.label();
    match e {
        CompletionError::RateLimitExceeded(msg) => {
            tracing::warn!(
                endpoint = label,
                "Concurrent request limit exceeded for privacy endpoint"
            );
            (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
        }
        CompletionError::ProviderError {
            status_code,
            message,
        } => {
            // Upstream 4xx keeps its status (the client sent something the
            // model rejected, so retrying will not help); auth failures and
            // 5xx become a generic 500. `message` is the pool's sanitized
            // "PII detector returned HTTP N", never the upstream body.
            let failure_message = match endpoint {
                PrivacyEndpoint::Classify => {
                    "Privacy classify request failed. Please try again later."
                }
                PrivacyEndpoint::Redact => "Privacy redact request failed. Please try again later.",
            };
            let classified = super::embeddings::classify_provider_error_with(
                status_code,
                message.clone(),
                failure_message,
            );
            if classified.0.is_client_error() {
                tracing::warn!(
                    endpoint = label,
                    upstream_status = status_code,
                    detail = %message,
                    "Privacy request rejected by upstream with client error"
                );
            } else {
                tracing::error!(
                    endpoint = label,
                    upstream_status = status_code,
                    detail = %message,
                    "Privacy provider error"
                );
            }
            classified
        }
        CompletionError::InvalidModel(msg) => {
            tracing::warn!(endpoint = label, "Privacy model not found");
            (StatusCode::NOT_FOUND, "not_found_error", msg)
        }
        CompletionError::ServiceOverloaded(_) => {
            tracing::warn!(endpoint = label, "Privacy service overloaded");
            (
                crate::routes::common::status_overloaded(),
                "service_overloaded",
                "All inference backends are overloaded. Please retry with exponential backoff."
                    .to_string(),
            )
        }
        _ => {
            tracing::error!(endpoint = label, "Unexpected privacy endpoint error");
            let message = match endpoint {
                PrivacyEndpoint::Classify => "Privacy classify request failed",
                PrivacyEndpoint::Redact => "Privacy redact request failed",
            };
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                message.to_string(),
            )
        }
    }
}

fn privacy_error_response(
    endpoint: PrivacyEndpoint,
    e: services::completions::ports::CompletionError,
) -> axum::response::Response {
    let (status_code, error_type, message) = privacy_error_parts(endpoint, e);
    (
        status_code,
        ResponseJson(ErrorResponse::new(message, error_type.to_string())),
    )
        .into_response()
}

/// One privacy-filter data entry's billable usage.
#[derive(Debug, PartialEq)]
struct PrivacyUsageEntry {
    /// The entry's `index`; `None` when present but not a valid index.
    index: Option<usize>,
    /// Non-negative `usage.input_tokens`.
    input_tokens: i64,
}

/// Billable usage per data entry of a privacy-filter response:
/// `{"data": [{"index": i, "spans": [...], "usage": {"input_tokens": N}}, ...]}`.
///
/// Lenient per entry, like `auto_redact::detect::parse_response`: an entry
/// whose `input_tokens` is absent, negative or not an integer is skipped
/// without voiding the others, so one malformed entry cannot zero the bill.
/// A missing `index` defaults to 0. A body without a `data` array yields
/// no entries.
fn privacy_usage_entries(response_bytes: &[u8]) -> Vec<PrivacyUsageEntry> {
    #[derive(serde::Deserialize)]
    struct UsageExtract {
        #[serde(default)]
        data: Vec<serde_json::Value>,
    }

    let Ok(parsed) = serde_json::from_slice::<UsageExtract>(response_bytes) else {
        return Vec::new();
    };
    parsed
        .data
        .iter()
        .filter_map(|entry| {
            let input_tokens = entry
                .get("usage")?
                .get("input_tokens")?
                .as_i64()
                .filter(|t| *t >= 0)?;
            let index = match entry.get("index") {
                None => Some(0),
                Some(v) => v.as_u64().and_then(|i| usize::try_from(i).ok()),
            };
            Some(PrivacyUsageEntry {
                index,
                input_tokens,
            })
        })
        .collect()
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
        (status = 429, description = "Rate limited or overloaded — retry with backoff. Check error.type: rate_limit_exceeded or service_overloaded.", body = ErrorResponse),
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
    Extension(correlation): Extension<RequestCorrelation>,
    headers: header::HeaderMap,
    requested_model: RequestedModel,
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
    requested_model.set(&model_name);

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
    insert_request_id_header(correlation, &mut extra);

    match app_state
        .completion_service
        .try_privacy_classify(organization_id, model_id, &model_name, body, extra)
        .await
    {
        Ok(response_bytes) => {
            // Sum every entry's input_tokens for billing. Saturating so a
            // provider returning huge or many counts cannot wrap before the
            // MAX_REASONABLE_TOKENS check below.
            let token_sum_i64: i64 = privacy_usage_entries(&response_bytes)
                .iter()
                .fold(0i64, |acc, e| acc.saturating_add(e.input_tokens));
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
                cache_write: None,
                profiled_cache_write_tokens: 0,
                requested_service_tier: None,
                provider_service_tier: None,
                inference_type: services::usage::ports::InferenceType::PrivacyClassify,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(inference_id),
                provider_request_id: None,
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
                discount: None,
                provider_attribution: services::usage::ProviderAttribution::default(),
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
        Err(e) => privacy_error_response(PrivacyEndpoint::Classify, e),
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
        (status = 429, description = "Rate limited or overloaded — retry with backoff. Check error.type: rate_limit_exceeded or service_overloaded.", body = ErrorResponse),
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
    Extension(correlation): Extension<RequestCorrelation>,
    headers: header::HeaderMap,
    requested_model: RequestedModel,
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
    requested_model.set(&model_name);

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
    insert_request_id_header(correlation, &mut extra);

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
        Err(e) => return privacy_error_response(PrivacyEndpoint::Redact, e),
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
    // Accumulate per index. The detector may split one input's detections
    // across several entries with the same `index` (see
    // `auto_redact::detect::parse_response`); each entry's usage is real
    // work, so sum them — as /privacy/classify does for this same upstream
    // call — and surface the per-input total. Out-of-range indexes and
    // negative counts are dropped.
    let mut per_index_tokens: std::collections::HashMap<usize, i64> =
        std::collections::HashMap::new();
    for entry in privacy_usage_entries(&response_bytes) {
        let Some(index) = entry.index.filter(|i| *i < texts.len()) else {
            continue;
        };
        let total = per_index_tokens.entry(index).or_insert(0);
        *total = total.saturating_add(entry.input_tokens);
    }
    let token_sum_i64: i64 = per_index_tokens
        .values()
        .fold(0i64, |acc, t| acc.saturating_add(*t));
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
        cache_write: None,
        profiled_cache_write_tokens: 0,
        requested_service_tier: None,
        provider_service_tier: None,
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
        discount: None,
        provider_attribution: services::usage::ProviderAttribution::default(),
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
            let usage_json = per_index_tokens
                .get(&idx)
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

#[cfg(test)]
mod tests {
    use super::*;
    use services::completions::ports::CompletionError;

    fn entry(index: Option<usize>, input_tokens: i64) -> PrivacyUsageEntry {
        PrivacyUsageEntry {
            index,
            input_tokens,
        }
    }

    #[test]
    fn usage_entries_skip_only_the_malformed_entry() {
        let body = br#"{"data": [
            {"index": 0, "usage": {"input_tokens": 5}},
            {"index": 1, "usage": {"input_tokens": "n/a"}},
            {"index": 2, "usage": {"input_tokens": 3000000000}},
            {"index": 3, "usage": {"input_tokens": -4}},
            {"index": 4, "usage": {"input_tokens": 1.5}},
            {"index": 5, "usage": {}},
            {"index": 6},
            {"usage": {"input_tokens": 2}},
            {"index": "x", "usage": {"input_tokens": 3}}
        ]}"#;
        assert_eq!(
            privacy_usage_entries(body),
            vec![
                entry(Some(0), 5),
                entry(Some(2), 3_000_000_000),
                entry(Some(0), 2),
                entry(None, 3),
            ]
        );
    }

    #[test]
    fn usage_entries_empty_for_unexpected_bodies() {
        assert!(privacy_usage_entries(b"not json").is_empty());
        assert!(privacy_usage_entries(br#"{"data": "nope"}"#).is_empty());
        assert!(privacy_usage_entries(br#"{"model": "m"}"#).is_empty());
    }

    fn parts(endpoint: PrivacyEndpoint, e: CompletionError) -> (u16, &'static str, String) {
        let (status, ty, msg) = privacy_error_parts(endpoint, e);
        (status.as_u16(), ty, msg)
    }

    #[test]
    fn rate_limit_is_429_for_both_endpoints() {
        for endpoint in [PrivacyEndpoint::Classify, PrivacyEndpoint::Redact] {
            assert_eq!(
                parts(
                    endpoint,
                    CompletionError::RateLimitExceeded("slow down".into())
                ),
                (429, "rate_limit_error", "slow down".to_string())
            );
        }
    }

    #[test]
    fn invalid_model_is_404_for_both_endpoints() {
        for endpoint in [PrivacyEndpoint::Classify, PrivacyEndpoint::Redact] {
            assert_eq!(
                parts(
                    endpoint,
                    CompletionError::InvalidModel("no such model".into())
                ),
                (404, "not_found_error", "no such model".to_string())
            );
        }
    }

    /// Redact used to answer 503 here, which its OpenAPI doc never declared
    /// and which gateways count as downtime. Both endpoints now follow the
    /// codebase convention (`status_overloaded()`, 429) with one message.
    #[test]
    fn overloaded_is_429_for_both_endpoints() {
        for endpoint in [PrivacyEndpoint::Classify, PrivacyEndpoint::Redact] {
            assert_eq!(
                parts(endpoint, CompletionError::ServiceOverloaded("busy".into())),
                (
                    429,
                    "service_overloaded",
                    "All inference backends are overloaded. Please retry with exponential backoff."
                        .to_string()
                )
            );
        }
    }

    /// Every ProviderError used to come back as `server_error` "try again
    /// later", even for an upstream 4xx caused by the client's own payload
    /// (classify forwards the body verbatim). It now goes through the same
    /// classification as embeddings: 4xx keeps its status as
    /// `invalid_request_error` (404 as `not_found_error`), while auth
    /// failures and 5xx collapse to a generic 500.
    #[test]
    fn provider_error_keeps_client_status_and_masks_the_rest() {
        let msg = |s: u16| format!("PII detector returned HTTP {s}");
        for (endpoint, generic) in [
            (
                PrivacyEndpoint::Classify,
                "Privacy classify request failed. Please try again later.",
            ),
            (
                PrivacyEndpoint::Redact,
                "Privacy redact request failed. Please try again later.",
            ),
        ] {
            for (status, expected) in [
                (400u16, (400u16, "invalid_request_error", msg(400))),
                (422, (422, "invalid_request_error", msg(422))),
                (404, (404, "not_found_error", msg(404))),
                (401, (500, "server_error", generic.to_string())),
                (407, (500, "server_error", generic.to_string())),
                (500, (500, "server_error", generic.to_string())),
                (502, (500, "server_error", generic.to_string())),
            ] {
                assert_eq!(
                    parts(
                        endpoint,
                        CompletionError::ProviderError {
                            status_code: status,
                            message: msg(status),
                        }
                    ),
                    expected,
                    "{endpoint:?} upstream {status}"
                );
            }
        }
    }

    #[test]
    fn other_errors_are_500_server_error() {
        assert_eq!(
            parts(
                PrivacyEndpoint::Classify,
                CompletionError::InternalError("boom".into())
            ),
            (
                500,
                "server_error",
                "Privacy classify request failed".to_string()
            )
        );
        assert_eq!(
            parts(
                PrivacyEndpoint::Redact,
                CompletionError::InvalidParams("bad".into())
            ),
            (
                500,
                "server_error",
                "Privacy redact request failed".to_string()
            )
        );
    }
}
