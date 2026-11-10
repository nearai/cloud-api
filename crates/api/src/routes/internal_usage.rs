use crate::{
    models::ErrorResponse,
    routes::{api::AppState, common::format_amount, usage::UsageError},
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json as ResponseJson,
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use utoipa::ToSchema;
use uuid::Uuid;

// ============================================
// Usage recording response
// ============================================

/// Usage-recording response body — tagged union matching the request type.
/// Returned by `POST /v1/internal/usage`. All costs use fixed scale of 9
/// (nano-dollars) and USD currency.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordUsageResponse {
    /// Response for token-based chat completion usage
    ChatCompletion {
        /// Unique ID of the recorded usage entry
        id: String,
        /// Model name
        model: String,
        /// Input tokens recorded
        input_tokens: i32,
        /// Output tokens recorded
        output_tokens: i32,
        /// Total tokens (input + output)
        total_tokens: i32,
        /// Number of prompt tokens that were cache hits (subset of input_tokens)
        cache_read_tokens: i32,
        /// Number of prompt tokens written to provider cache (subset of input_tokens)
        cache_write_tokens: i32,
        /// Input cost in nano-dollars (scale 9)
        input_cost: i64,
        /// Output cost in nano-dollars (scale 9)
        output_cost: i64,
        /// Total cost in nano-dollars (scale 9)
        total_cost: i64,
        /// Human-readable total cost (e.g., "$0.00123")
        total_cost_display: String,
        /// Timestamp of the recorded entry (RFC3339)
        created_at: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        credit_allocations: Option<Vec<services::usage::CreditAllocation>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        funded_amount: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unfunded_amount: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        allocation_policy_version: Option<String>,
    },
    /// Response for image generation usage
    ImageGeneration {
        /// Unique ID of the recorded usage entry
        id: String,
        /// Model name
        model: String,
        /// Number of images generated
        image_count: i32,
        /// Total cost in nano-dollars (scale 9)
        total_cost: i64,
        /// Human-readable total cost (e.g., "$5.00")
        total_cost_display: String,
        /// Timestamp of the recorded entry (RFC3339)
        created_at: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        credit_allocations: Option<Vec<services::usage::CreditAllocation>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        funded_amount: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unfunded_amount: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        allocation_policy_version: Option<String>,
    },
}

/// Build the public `RecordUsageResponse` from a service-layer `UsageLogEntry`.
///
/// The variant is chosen by the *returned entry's* `inference_type` rather
/// than the caller's request type so that an idempotent duplicate (which
/// returns the existing row, possibly recorded under a different type) is
/// still serialized correctly.
fn build_record_usage_response(entry: services::usage::UsageLogEntry) -> RecordUsageResponse {
    match entry.inference_type {
        services::usage::InferenceType::ImageGeneration
        | services::usage::InferenceType::ImageEdit => RecordUsageResponse::ImageGeneration {
            id: entry.id.to_string(),
            model: entry.model,
            image_count: entry.image_count.unwrap_or_else(|| {
                tracing::error!(
                    entry_id = %entry.id,
                    "image_count unexpectedly missing for image generation usage record"
                );
                0
            }),
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            created_at: entry.created_at.to_rfc3339(),
            credit_allocations: entry.credit_allocations,
            funded_amount: entry.funded_amount,
            unfunded_amount: entry.unfunded_amount,
            allocation_policy_version: entry.allocation_policy_version,
        },
        _ => RecordUsageResponse::ChatCompletion {
            id: entry.id.to_string(),
            model: entry.model,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
            total_tokens: entry.total_tokens,
            cache_read_tokens: entry.cache_read_tokens,
            cache_write_tokens: entry.cache_write_tokens,
            input_cost: entry.input_cost,
            output_cost: entry.output_cost,
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            created_at: entry.created_at.to_rfc3339(),
            credit_allocations: entry.credit_allocations,
            funded_amount: entry.funded_amount,
            unfunded_amount: entry.unfunded_amount,
            allocation_policy_version: entry.allocation_policy_version,
        },
    }
}

// =====================================================================
// POST /v1/internal/usage — Service-token-authenticated usage recording
// =====================================================================
//
// This endpoint is the sole usage-recording entry point for trusted
// infrastructure reporters (today: inference-proxy). It replaced the
// removed `sk-…`-authenticated `POST /v1/usage` — which let any holder of
// that key submit arbitrary usage rows on the org's behalf — and instead
// requires a shared `CLOUD_API_USAGE_TOKEN` service secret and carries the
// subject identity (`organization_id`, `workspace_id`, `api_key_id`) in
// the body. The body may also carry `discount_to_user`, the fraction an
// aggregator lane publishes in its models document; it is applied after
// catalog pricing and capped by `INTERNAL_USAGE_MAX_DISCOUNT`.
//
// Threat model:
// - Anyone holding `CLOUD_API_USAGE_TOKEN` can submit usage rows for any
//   org. The token therefore lives only on trusted infrastructure
//   (inference-proxy CVMs) and is rotated alongside other service secrets.
//
// The body shape is the existing `RecordUsageApiRequest` flattened under a
// wrapper that adds the three identity fields and the optional
// `discount_to_user`, so reporters keep one builder.

/// Request body for `POST /v1/internal/usage`. The `usage` field is
/// flattened, so the on-the-wire shape is `RecordUsageApiRequest` JSON with
/// four extra top-level keys: the three identity fields and the optional
/// `discount_to_user`.
///
/// `ToSchema` is intentionally not derived: `RecordUsageApiRequest`
/// doesn't implement `PartialSchema` (its OpenAPI doc was hand-rolled via
/// `request_body = serde_json::Value`), and `#[serde(flatten)]` requires
/// the inner type to be a known schema. Internal endpoints aren't surfaced
/// in the public OpenAPI doc anyway.
#[derive(Debug, Deserialize)]
pub struct RecordUsageInternalRequest {
    pub admission_id: Option<Uuid>,
    /// UUID of the organization to bill. **Trusted as provided** —
    /// `record_usage_from_api` does not cross-validate that this org
    /// owns the supplied `workspace_id` or `api_key_id`, matching the
    /// stated threat model (any holder of `CLOUD_API_USAGE_TOKEN` is
    /// allowed to write rows on behalf of any tenant). Reporters must
    /// supply mutually consistent values; cloud-api enforces only
    /// existence of the model named in the inner `usage` payload.
    pub organization_id: String,
    /// UUID of the workspace the usage belongs to. Trusted as provided
    /// (see `organization_id`).
    pub workspace_id: String,
    /// UUID of the API key the usage should be attributed to (for
    /// per-key analytics). Trusted as provided.
    pub api_key_id: String,
    /// Optional provider-side discount as the fraction of the list price the
    /// subject is *not* charged (`0.2` = 20% off), mirroring the
    /// `discount_to_user` an aggregator lane publishes in its models
    /// document. Applied to every priced component after catalog pricing;
    /// the list amounts are kept in the row's `billing_details`. Must be in
    /// `[0, 1)`, be a multiple of `0.0001` (whole basis points) and not
    /// exceed `INTERNAL_USAGE_MAX_DISCOUNT`. Omitted or `0` bills at list price.
    pub discount_to_user: Option<f64>,
    /// The standard usage payload.
    #[serde(flatten)]
    pub usage: services::usage::RecordUsageApiRequest,
}

/// Turn the optional `discount_to_user` fraction into a validated
/// [`services::usage::UsageDiscount`], enforcing the operator ceiling
/// `INTERNAL_USAGE_MAX_DISCOUNT` so a misconfigured reporter cannot record
/// rows far below list price. `None` and `0` mean list price.
fn resolve_internal_usage_discount(
    discount_to_user: Option<f64>,
    max_discount: f64,
) -> Result<Option<services::usage::UsageDiscount>, UsageError> {
    let Some(fraction) = discount_to_user else {
        return Ok(None);
    };
    let validation_error = |message: String| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(message, "validation_error".to_string())),
        )
    };
    let discount = services::usage::UsageDiscount::from_fraction(fraction)
        .map_err(|error| validation_error(error.to_string()))?;
    if let Some(discount) = discount {
        // Compare in whole basis points so a ceiling like `0.2` accepts
        // exactly 0.2. The ceiling is floored (with a tolerance for float
        // representation) so a value between two basis points never admits
        // the one above it.
        let ceiling_bp = (max_discount * 10_000.0 + 1e-6).floor();
        if f64::from(discount.basis_points()) > ceiling_bp {
            return Err(validation_error(format!(
                "discount_to_user {} exceeds the configured maximum {}",
                discount.fraction(),
                max_discount
            )));
        }
    }
    Ok(discount)
}

/// Validate the `Authorization: Bearer …` header against the configured
/// `internal_usage_token` in constant time. Shared by every `/v1/internal/*`
/// route. Returns:
/// - `Ok(())` on match.
/// - `Err(503)` if the endpoint is disabled (token not configured).
/// - `Err(401)` if the header is missing or doesn't match.
pub(crate) fn verify_internal_usage_token(
    headers: &HeaderMap,
    expected: Option<&str>,
) -> Result<(), UsageError> {
    let expected = expected.ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            ResponseJson(ErrorResponse::new(
                "Internal endpoint is not configured on this deployment".to_string(),
                "endpoint_disabled".to_string(),
            )),
        )
    })?;
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    let unauthorized = || {
        (
            StatusCode::UNAUTHORIZED,
            ResponseJson(ErrorResponse::new(
                "Invalid or missing service token".to_string(),
                "unauthorized".to_string(),
            )),
        )
    };
    let provided = provided.ok_or_else(unauthorized)?;
    if provided.as_bytes().ct_eq(expected.as_bytes()).into() {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

/// Record usage from a trusted internal reporter.
///
/// Auth: `Authorization: Bearer <CLOUD_API_USAGE_TOKEN>`. The endpoint is
/// disabled (503) until that env var is set on the cloud-api side.
pub async fn record_usage_internal(
    State(app_state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<ResponseJson<RecordUsageResponse>, UsageError> {
    // Auth before body deserialization. Axum extractors run in signature
    // order, so passing `Json<…>` directly would parse the body before
    // any auth check — meaning a disabled-endpoint + malformed-body
    // request would return 422 instead of 503, and unauthenticated
    // callers could probe the body schema via parse errors. We take
    // raw `Bytes` and deserialize only after `verify_internal_usage_token`
    // succeeds, preserving the fail-closed posture.
    verify_internal_usage_token(&headers, app_state.config.internal_usage_token.as_deref())?;

    let request: RecordUsageInternalRequest = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                format!("Invalid request body: {e}"),
                "validation_error".to_string(),
            )),
        )
    })?;

    let parse_uuid = |raw: &str, field: &'static str| {
        Uuid::parse_str(raw).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    format!("Invalid {field}: must be a UUID"),
                    "validation_error".to_string(),
                )),
            )
        })
    };
    let organization_id = parse_uuid(&request.organization_id, "organization_id")?;
    let workspace_id = parse_uuid(&request.workspace_id, "workspace_id")?;
    let api_key_id = parse_uuid(&request.api_key_id, "api_key_id")?;
    let discount = resolve_internal_usage_discount(
        request.discount_to_user,
        app_state.config.internal_usage_max_discount,
    )?;

    let entry = app_state
        .usage_service
        .record_usage_from_api(
            organization_id,
            workspace_id,
            api_key_id,
            request.usage,
            discount,
            request.admission_id,
        )
        .await
        .map_err(|e| match &e {
            services::usage::UsageError::ModelNotFound(_) => (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(e.to_string(), "not_found".to_string())),
            ),
            services::usage::UsageError::ValidationError(_) => (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    e.to_string(),
                    "validation_error".to_string(),
                )),
            ),
            _ => {
                tracing::error!(
                    %organization_id,
                    %workspace_id,
                    %api_key_id,
                    "Failed to record internal usage"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to record usage".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    Ok(ResponseJson(build_record_usage_response(entry)))
}

/// Metadata-only admission for trusted execution gateways. It remains mandatory when their
/// usage receipts travel by SQS: cached key authorization never authorizes monetary spending.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReserveUsageRequest {
    execution_model: Option<String>,
    id: Uuid,
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model: String,
    features: services::usage::budget::UsageFeatures,
}

pub async fn reserve_usage_internal(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<ResponseJson<serde_json::Value>, UsageError> {
    verify_internal_usage_token(&headers, state.config.internal_usage_token.as_deref())?;
    let failure = |status, message: &str| {
        (
            status,
            ResponseJson(ErrorResponse::new(
                message.into(),
                "budget_admission_error".into(),
            )),
        )
    };
    let request: ReserveUsageRequest = serde_json::from_slice(&body)
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "Invalid admission metadata"))?;
    if request.id.is_nil() {
        return Err(failure(StatusCode::BAD_REQUEST, "Invalid execution id"));
    }
    let model = state
        .models_service
        .resolve_and_get_model(
            &request.model,
            services::models::ModelCaller::Organization(request.organization_id),
        )
        .await
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "Model unavailable"))?;
    // Fusion child endpoints may use model IDs outside this catalog. Their optional
    // metadata affects only the estimate; the outer billed model remains authorized.
    let output_hint = if let Some(name) = request.execution_model.as_deref() {
        state
            .models_service
            .resolve_and_get_model(
                name,
                services::models::ModelCaller::Organization(request.organization_id),
            )
            .await
            .ok()
            .and_then(|model| model.max_output_length)
    } else {
        model.max_output_length
    };
    let features = request.features.with_model_output(output_hint);
    state
        .usage_service
        .admit_external(
            services::usage::BillingIdentity {
                organization_id: request.organization_id,
                workspace_id: request.workspace_id,
                api_key_id: request.api_key_id,
                model_id: model.id,
            },
            features,
            request.id,
        )
        .await
        .map_err(|error| match error {
            services::usage::UsageError::LimitExceeded(_) => failure(
                StatusCode::PAYMENT_REQUIRED,
                "Insufficient available budget",
            ),
            services::usage::UsageError::Unauthorized(_) => {
                failure(StatusCode::FORBIDDEN, "Billing identity unavailable")
            }
            services::usage::UsageError::ValidationError(_)
            | services::usage::UsageError::CostCalculationOverflow(_) => {
                failure(StatusCode::BAD_REQUEST, "Invalid admission estimate")
            }
            _ => failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "Budget admission unavailable",
            ),
        })?;
    Ok(ResponseJson(
        serde_json::json!({"id": request.id, "model": model.model_name}),
    ))
}

#[cfg(test)]
mod internal_usage_tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    #[test]
    fn verify_returns_503_when_endpoint_unconfigured() {
        // Default deployment posture: usage recording is disabled until the
        // operator sets CLOUD_API_USAGE_TOKEN. There is no longer a legacy
        // fallback, so reporters simply cannot submit usage until then.
        let h = headers_with_bearer("anything");
        let err = verify_internal_usage_token(&h, None).unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn verify_returns_401_on_missing_header() {
        let err = verify_internal_usage_token(&HeaderMap::new(), Some("svc")).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_returns_401_on_wrong_token() {
        let h = headers_with_bearer("not-the-right-one");
        let err = verify_internal_usage_token(&h, Some("the-actual-secret")).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_returns_401_on_missing_bearer_prefix() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("svc-token"));
        let err = verify_internal_usage_token(&h, Some("svc-token")).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn discount_absent_or_zero_means_list_price() {
        assert_eq!(resolve_internal_usage_discount(None, 0.5).unwrap(), None);
        assert_eq!(
            resolve_internal_usage_discount(Some(0.0), 0.5).unwrap(),
            None
        );
        // A ceiling of zero still lets a reporter send an explicit `0`.
        assert_eq!(
            resolve_internal_usage_discount(Some(0.0), 0.0).unwrap(),
            None
        );
    }

    #[test]
    fn discount_within_ceiling_is_accepted() {
        let discount = resolve_internal_usage_discount(Some(0.2), 0.5)
            .unwrap()
            .expect("20% is a discount");
        assert_eq!(discount.basis_points(), 2_000);
        // Exactly at the ceiling is allowed.
        let at_ceiling = resolve_internal_usage_discount(Some(0.2), 0.2)
            .unwrap()
            .expect("20% is a discount");
        assert_eq!(at_ceiling.basis_points(), 2_000);
    }

    #[test]
    fn discount_outside_unit_interval_is_a_400() {
        for bad in [1.0, 1.5, -0.1, f64::NAN, f64::INFINITY] {
            let err = resolve_internal_usage_discount(Some(bad), 0.9).unwrap_err();
            assert_eq!(err.0, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(err.1 .0.error.r#type, "validation_error");
        }
    }

    #[test]
    fn discount_finer_than_a_basis_point_is_a_400() {
        let err = resolve_internal_usage_discount(Some(0.12345), 0.9).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            err.1 .0.error.message.contains("multiple of 0.0001"),
            "{}",
            err.1 .0.error.message
        );
    }

    #[test]
    fn discount_above_ceiling_is_a_400() {
        let err = resolve_internal_usage_discount(Some(0.3), 0.25).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            err.1 .0.error.message.contains("exceeds"),
            "{}",
            err.1 .0.error.message
        );
        // A ceiling of zero refuses every non-zero discount.
        let err = resolve_internal_usage_discount(Some(0.0001), 0.0).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn internal_request_body_carries_discount_to_user() {
        // `usage` is flattened, so pin that the sibling field survives serde's
        // flatten buffering for both a float and an integer zero.
        let mut body = serde_json::json!({
            "organization_id": "11111111-1111-1111-1111-111111111111",
            "workspace_id": "22222222-2222-2222-2222-222222222222",
            "api_key_id": "33333333-3333-3333-3333-333333333333",
            "type": "chat_completion",
            "model": "m",
            "input_tokens": 1,
            "output_tokens": 1,
            "id": "x",
            "discount_to_user": 0.2
        });
        let parsed: super::RecordUsageInternalRequest =
            serde_json::from_value(body.clone()).unwrap();
        assert_eq!(parsed.discount_to_user, Some(0.2));
        body["discount_to_user"] = serde_json::json!(0);
        let parsed: super::RecordUsageInternalRequest =
            serde_json::from_value(body.clone()).unwrap();
        assert_eq!(parsed.discount_to_user, Some(0.0));
        body.as_object_mut().unwrap().remove("discount_to_user");
        let parsed: super::RecordUsageInternalRequest = serde_json::from_value(body).unwrap();
        assert_eq!(parsed.discount_to_user, None);
    }

    #[test]
    fn discount_ceiling_between_basis_points_floors() {
        // 0.10005 is 1000.5 bp: 0.1001 must be refused, 0.1 accepted.
        let err = resolve_internal_usage_discount(Some(0.1001), 0.10005).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let ok = resolve_internal_usage_discount(Some(0.1), 0.10005)
            .unwrap()
            .expect("10% is a discount");
        assert_eq!(ok.basis_points(), 1_000);
    }

    #[test]
    fn verify_accepts_matching_token() {
        let h = headers_with_bearer("the-actual-secret");
        assert!(verify_internal_usage_token(&h, Some("the-actual-secret")).is_ok());
    }

    #[test]
    fn verify_rejects_close_but_not_equal_tokens() {
        // Sanity that we're comparing the whole string, not a prefix.
        let h = headers_with_bearer("svc-token-tampered");
        let err = verify_internal_usage_token(&h, Some("svc-token")).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }
}
