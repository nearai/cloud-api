use crate::{
    middleware::AuthenticatedUser,
    models::ErrorResponse,
    routes::{api::AppState, common::format_amount},
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json as ResponseJson,
    Extension,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use services::{organization::OrganizationError, usage::UsageServiceTrait};
use subtle::ConstantTimeEq;
use utoipa::ToSchema;
use uuid::Uuid;

pub(crate) type UsageError = (StatusCode, ResponseJson<ErrorResponse>);

async fn check_org_membership(
    app_state: &AppState,
    user: AuthenticatedUser,
    org_id: &str,
) -> Result<Uuid, UsageError> {
    let organization_id = Uuid::parse_str(org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let user_id = crate::conversions::authenticated_user_to_user_id(user);
    let is_member = app_state
        .organization_service
        .is_member(
            services::organization::OrganizationId(organization_id),
            user_id,
        )
        .await
        .map_err(|e| match e {
            OrganizationError::NotFound => (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    "Organization not found".to_string(),
                    "not_found".to_string(),
                )),
            ),
            _ => {
                tracing::error!("Failed to check organization membership");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to verify organization access".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    if !is_member {
        return Err((
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "You are not authorized to access this organization.".to_string(),
                "forbidden".to_string(),
            )),
        ));
    }

    Ok(organization_id)
}

/// Get organization balance response
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrganizationBalanceResponse {
    pub organization_id: String,
    pub total_spent: i64,            // In nano-dollars (scale 9)
    pub total_spent_display: String, // Human readable, e.g., "$12.50"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_limit: Option<i64>, // In nano-dollars (scale 9), None if not set
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_limit_display: Option<String>, // Human readable, e.g., "$100.00"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<i64>, // Remaining credits in nano-dollars
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_display: Option<String>, // Human readable remaining
    pub last_usage_at: Option<String>,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub updated_at: String,
}

pub async fn compute_organization_balance_response(
    usage_service: &(dyn UsageServiceTrait + Send + Sync),
    organization_id: Uuid,
) -> Result<OrganizationBalanceResponse, UsageError> {
    let (balance, limit) = tokio::try_join!(
        usage_service.get_balance(organization_id),
        usage_service.get_limit(organization_id)
    )
    .map_err(|e| {
        tracing::error!("Failed to get organization balance or limit: {:?}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                "Failed to retrieve balance or limit".to_string(),
                "internal_server_error".to_string(),
            )),
        )
    })?;

    match balance {
        Some(balance) => {
            let (spend_limit, spend_limit_display, remaining, remaining_display) =
                if let Some(limit_info) = limit {
                    let remaining_amount = limit_info.spend_limit - balance.total_spent;
                    (
                        Some(limit_info.spend_limit),
                        Some(format_amount(limit_info.spend_limit)),
                        Some(remaining_amount),
                        Some(format_amount(remaining_amount)),
                    )
                } else {
                    (None, None, None, None)
                };

            Ok(OrganizationBalanceResponse {
                organization_id: balance.organization_id.to_string(),
                total_spent: balance.total_spent,
                total_spent_display: format_amount(balance.total_spent),
                spend_limit,
                spend_limit_display,
                remaining,
                remaining_display,
                last_usage_at: balance.last_usage_at.map(|dt| dt.to_rfc3339()),
                total_requests: balance.total_requests,
                total_tokens: balance.total_tokens,
                updated_at: balance.updated_at.to_rfc3339(),
            })
        }
        None => {
            if let Some(limit_info) = limit {
                Ok(OrganizationBalanceResponse {
                    organization_id: organization_id.to_string(),
                    total_spent: 0,
                    total_spent_display: format_amount(0),
                    spend_limit: Some(limit_info.spend_limit),
                    spend_limit_display: Some(format_amount(limit_info.spend_limit)),
                    remaining: Some(limit_info.spend_limit),
                    remaining_display: Some(format_amount(limit_info.spend_limit)),
                    last_usage_at: None,
                    total_requests: 0,
                    total_tokens: 0,
                    updated_at: Utc::now().to_rfc3339(),
                })
            } else {
                Err((
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        "No usage data or limit found for organization".to_string(),
                        "not_found".to_string(),
                    )),
                ))
            }
        }
    }
}

/// Usage history entry
/// All costs use fixed scale of 9 (nano-dollars) and USD currency.
/// `cache_read_tokens` is meaningful only for token-based chat-style models; for other
/// inference types (e.g., rerank, audio transcription, image), it will typically be 0
/// and is not used in billing calculations.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageHistoryEntryResponse {
    pub id: String,
    pub workspace_id: String,
    pub api_key_id: String,
    pub model: String, // Model name (canonical name from models table)
    pub input_tokens: i32,
    pub output_tokens: i32,
    /// Number of prompt tokens that were cache hits
    pub cache_read_tokens: i32,
    pub total_tokens: i32,
    pub total_cost: i64,            // In nano-dollars (scale 9)
    pub total_cost_display: String, // Human readable, e.g., "$0.00123"
    pub inference_type: String,
    pub created_at: String,
    /// Why the inference ended (e.g., "completed", "length", "client_disconnect")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Response ID when called from Responses API
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Raw request ID from the inference provider
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    /// Inference UUID (hashed from provider_request_id)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_id: Option<String>,
    /// Number of images generated (for image generation requests)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_count: Option<i32>,
}

/// Usage history response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageHistoryResponse {
    pub data: Vec<UsageHistoryEntryResponse>,
    pub total: usize,
    pub limit: i64,
    pub offset: i64,
}

/// Query parameters for usage history
#[derive(Debug, Deserialize)]
pub struct UsageHistoryQuery {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// Service usage history entry (platform services like web_search).
/// All costs use fixed scale of 9 (nano-dollars) and USD currency.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ServiceUsageEntryResponse {
    pub id: String,
    pub organization_id: String,
    pub workspace_id: String,
    pub api_key_id: String,
    pub service_id: String,
    pub quantity: i32,
    pub total_cost: i64,            // In nano-dollars (scale 9)
    pub total_cost_display: String, // Human readable, e.g., "$0.00123"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_id: Option<String>,
    pub created_at: String,
}

/// Service usage history response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ServiceUsageHistoryResponse {
    pub data: Vec<ServiceUsageEntryResponse>,
    pub total: usize,
    pub limit: i64,
    pub offset: i64,
}

/// Query parameters for service usage history
#[derive(Debug, Deserialize)]
pub struct ServiceUsageHistoryQuery {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    /// Filter by platform service name (e.g. \"web_search\").
    #[serde(rename = "serviceName")]
    pub service_name: Option<String>,
}

/// Get organization balance
///
/// Returns the current spending balance for an organization
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/balance",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization balance", body = OrganizationBalanceResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_balance(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
) -> Result<ResponseJson<OrganizationBalanceResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Get balance request for org {} by user {}",
        org_id,
        user.0.id
    );

    let organization_id = check_org_membership(&app_state, user, &org_id).await?;

    compute_organization_balance_response(&*app_state.usage_service, organization_id)
        .await
        .map(ResponseJson)
}

/// Get organization usage history
///
/// Returns paginated usage history for an organization
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/history",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID"),
        ("limit" = Option<i64>, Query, description = "Number of records to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Offset for pagination (default: 0)")
    ),
    responses(
        (status = 200, description = "Usage history", body = UsageHistoryResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_usage_history(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    Query(query): Query<UsageHistoryQuery>,
) -> Result<ResponseJson<UsageHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Get usage history for org {} by user {}, limit: {}, offset: {}",
        org_id,
        user.0.id,
        query.limit,
        query.offset
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(query.limit, query.offset)?;

    let organization_id = check_org_membership(&app_state, user, &org_id).await?;

    let (history, total) = app_state
        .usage_service
        .get_usage_history(organization_id, Some(query.limit), Some(query.offset))
        .await
        .map_err(|_| {
            tracing::error!("Failed to get usage history");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve usage history".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    let data = history
        .into_iter()
        .map(|entry| UsageHistoryEntryResponse {
            id: entry.id.to_string(),
            workspace_id: entry.workspace_id.to_string(),
            api_key_id: entry.api_key_id.to_string(),
            model: entry.model,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
            cache_read_tokens: entry.cache_read_tokens,
            total_tokens: entry.total_tokens,
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            inference_type: entry.inference_type.to_string(),
            created_at: entry.created_at.to_rfc3339(),
            stop_reason: entry.stop_reason.map(|r| r.to_string()),
            response_id: entry.response_id.map(|id| id.to_string()),
            provider_request_id: entry.provider_request_id,
            inference_id: entry.inference_id.map(|id| id.to_string()),
            image_count: entry.image_count,
        })
        .collect();

    Ok(ResponseJson(UsageHistoryResponse {
        data,
        total: total as usize,
        limit: query.limit,
        offset: query.offset,
    }))
}

/// Get service usage history for an organization
///
/// Returns paginated service usage logs (e.g., web_search) for an organization.
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/service-usage/history",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID"),
        ("serviceName" = Option<String>, Query, description = "Filter by platform service name (e.g. web_search)"),
        ("limit" = Option<i64>, Query, description = "Number of records to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Offset for pagination (default: 0)")
    ),
    responses(
        (status = 200, description = "Service usage history", body = ServiceUsageHistoryResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_service_usage_history(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    Query(query): Query<ServiceUsageHistoryQuery>,
) -> Result<ResponseJson<ServiceUsageHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Get service usage history for org {} by user {}, service: {:?}, limit: {}, offset: {}",
        org_id,
        user.0.id,
        query.service_name,
        query.limit,
        query.offset
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(query.limit, query.offset)?;

    let organization_id = check_org_membership(&app_state, user, &org_id).await?;

    let service_name = query.service_name.as_deref();

    let (history, total) = app_state
        .service_usage_service
        .get_usage_history(organization_id, service_name, query.limit, query.offset)
        .await
        .map_err(|_| {
            tracing::error!("Failed to get service usage history");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve service usage history".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    let data = history
        .into_iter()
        .map(|entry| ServiceUsageEntryResponse {
            id: entry.id.to_string(),
            organization_id: entry.organization_id.to_string(),
            workspace_id: entry.workspace_id.to_string(),
            api_key_id: entry.api_key_id.to_string(),
            service_id: entry.service_id.to_string(),
            quantity: entry.quantity,
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            inference_id: entry.inference_id.map(|id| id.to_string()),
            created_at: entry.created_at.to_rfc3339(),
        })
        .collect();

    Ok(ResponseJson(ServiceUsageHistoryResponse {
        data,
        total: total as usize,
        limit: query.limit,
        offset: query.offset,
    }))
}

/// Get API key usage history
///
/// Returns paginated usage history for a specific API key
#[utoipa::path(
    get,
    path = "/v1/workspaces/{workspace_id}/api-keys/{api_key_id}/usage/history",
    tag = "Usage",
    params(
        ("workspace_id" = String, Path, description = "Workspace ID"),
        ("api_key_id" = String, Path, description = "API Key ID"),
        ("limit" = Option<i64>, Query, description = "Number of records to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Offset for pagination (default: 0)")
    ),
    responses(
        (status = 200, description = "Usage history", body = UsageHistoryResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_api_key_usage_history(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((workspace_id, api_key_id)): Path<(String, String)>,
    Query(query): Query<UsageHistoryQuery>,
) -> Result<ResponseJson<UsageHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Get usage history for API key {} in workspace {} by user {}, limit: {}, offset: {}",
        api_key_id,
        workspace_id,
        user.0.id,
        query.limit,
        query.offset
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(query.limit, query.offset)?;

    let workspace_uuid = Uuid::parse_str(&workspace_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid workspace ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let api_key_uuid = Uuid::parse_str(&api_key_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid API key ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Get usage history with permission checking handled by the service
    let (history, total) = app_state
        .usage_service
        .get_api_key_usage_history_with_permissions(
            workspace_uuid,
            api_key_uuid,
            user.0.id,
            Some(query.limit),
            Some(query.offset),
        )
        .await
        .map_err(|e| {
            tracing::error!("Failed to get usage history");
            match e {
                services::usage::UsageError::Unauthorized(_) => (
                    StatusCode::FORBIDDEN,
                    ResponseJson(ErrorResponse::new(
                        "Access denied to this workspace".to_string(),
                        "forbidden".to_string(),
                    )),
                ),
                services::usage::UsageError::NotFound(_) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        "API key not found in this workspace".to_string(),
                        "not_found".to_string(),
                    )),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve usage history".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let data = history
        .into_iter()
        .map(|entry| UsageHistoryEntryResponse {
            id: entry.id.to_string(),
            workspace_id: entry.workspace_id.to_string(),
            api_key_id: entry.api_key_id.to_string(),
            model: entry.model,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
            cache_read_tokens: entry.cache_read_tokens,
            total_tokens: entry.total_tokens,
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            inference_type: entry.inference_type.to_string(),
            created_at: entry.created_at.to_rfc3339(),
            stop_reason: entry.stop_reason.map(|r| r.to_string()),
            response_id: entry.response_id.map(|id| id.to_string()),
            provider_request_id: entry.provider_request_id,
            inference_id: entry.inference_id.map(|id| id.to_string()),
            image_count: entry.image_count,
        })
        .collect();

    Ok(ResponseJson(UsageHistoryResponse {
        data,
        total: total as usize,
        limit: query.limit,
        offset: query.offset,
    }))
}

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
        },
        _ => RecordUsageResponse::ChatCompletion {
            id: entry.id.to_string(),
            model: entry.model,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
            total_tokens: entry.total_tokens,
            cache_read_tokens: entry.cache_read_tokens,
            input_cost: entry.input_cost,
            output_cost: entry.output_cost,
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            created_at: entry.created_at.to_rfc3339(),
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
// the body.
//
// Threat model:
// - Anyone holding `CLOUD_API_USAGE_TOKEN` can submit usage rows for any
//   org. The token therefore lives only on trusted infrastructure
//   (inference-proxy CVMs) and is rotated alongside other service secrets.
//
// The body shape is the existing `RecordUsageApiRequest` flattened under a
// wrapper that adds the three identity fields, so reporters keep one builder.

/// Request body for `POST /v1/internal/usage`. The `usage` field is
/// flattened, so the on-the-wire shape is `RecordUsageApiRequest` JSON with
/// three extra top-level keys.
///
/// `ToSchema` is intentionally not derived: `RecordUsageApiRequest`
/// doesn't implement `PartialSchema` (its OpenAPI doc was hand-rolled via
/// `request_body = serde_json::Value`), and `#[serde(flatten)]` requires
/// the inner type to be a known schema. Internal endpoints aren't surfaced
/// in the public OpenAPI doc anyway.
#[derive(Debug, Deserialize)]
pub struct RecordUsageInternalRequest {
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
    /// The standard usage payload.
    #[serde(flatten)]
    pub usage: services::usage::RecordUsageApiRequest,
}

/// Validate the `Authorization: Bearer …` header against the configured
/// `internal_usage_token` in constant time. Returns:
/// - `Ok(())` on match.
/// - `Err(503)` if the endpoint is disabled (token not configured).
/// - `Err(401)` if the header is missing or doesn't match.
fn verify_internal_usage_token(
    headers: &HeaderMap,
    expected: Option<&str>,
) -> Result<(), UsageError> {
    let expected = expected.ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            ResponseJson(ErrorResponse::new(
                "Internal usage endpoint is not configured on this deployment".to_string(),
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

    let entry = app_state
        .usage_service
        .record_usage_from_api(organization_id, workspace_id, api_key_id, request.usage)
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

// ============= User-facing analytics endpoints =============

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserMetricsSummary {
    pub total_requests: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cost_usd: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserModelMetrics {
    pub model_name: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserWorkspaceMetrics {
    pub workspace_id: String,
    pub workspace_name: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserOrganizationMetrics {
    pub organization_id: String,
    pub period_start: String,
    pub period_end: String,
    pub summary: UserMetricsSummary,
    pub by_model: Vec<UserModelMetrics>,
    pub by_workspace: Vec<UserWorkspaceMetrics>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserTimeSeriesPoint {
    pub date: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserTimeSeriesMetrics {
    pub organization_id: String,
    pub period_start: String,
    pub period_end: String,
    pub granularity: String,
    pub data: Vec<UserTimeSeriesPoint>,
}

#[derive(Debug, Deserialize)]
pub struct MetricsQuery {
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TimeSeriesQuery {
    pub start: Option<String>,
    pub end: Option<String>,
    pub granularity: Option<String>,
}

const MAX_DATE_RANGE_DAYS: i64 = 366;

fn validate_date_range(
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
) -> Result<(), UsageError> {
    if start >= end {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "start must be before end".to_string(),
                "invalid_date_range".to_string(),
            )),
        ));
    }
    if end - start > Duration::days(MAX_DATE_RANGE_DAYS) {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                format!("Date range must not exceed {MAX_DATE_RANGE_DAYS} days."),
                "date_range_too_large".to_string(),
            )),
        ));
    }
    Ok(())
}

fn parse_datetime_or_default(
    value: &Option<String>,
    default: chrono::DateTime<Utc>,
) -> Result<chrono::DateTime<Utc>, (StatusCode, ResponseJson<ErrorResponse>)> {
    match value {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        "Invalid date format. Use ISO 8601 (e.g. 2024-01-01T00:00:00Z)."
                            .to_string(),
                        "invalid_date".to_string(),
                    )),
                )
            }),
        None => Ok(default),
    }
}

#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/metrics",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID"),
        ("start" = Option<String>, Query, description = "Start date (ISO 8601, default: 30 days ago)"),
        ("end" = Option<String>, Query, description = "End date (ISO 8601, default: now)")
    ),
    responses(
        (status = 200, description = "Organization usage metrics", body = UserOrganizationMetrics),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_user_organization_metrics(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    Query(query): Query<MetricsQuery>,
) -> Result<ResponseJson<UserOrganizationMetrics>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let organization_id = check_org_membership(&app_state, user, &org_id).await?;

    let now = Utc::now();
    let end = parse_datetime_or_default(&query.end, now)?;
    let start = parse_datetime_or_default(&query.start, end - Duration::days(30))?;

    validate_date_range(start, end)?;

    let metrics = app_state
        .analytics_service
        .get_organization_metrics(organization_id, start, end)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get organization metrics: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve organization metrics".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok(ResponseJson(UserOrganizationMetrics {
        organization_id: metrics.organization_id.to_string(),
        period_start: metrics.period_start.to_rfc3339(),
        period_end: metrics.period_end.to_rfc3339(),
        summary: UserMetricsSummary {
            total_requests: metrics.summary.total_requests,
            total_input_tokens: metrics.summary.total_input_tokens,
            total_output_tokens: metrics.summary.total_output_tokens,
            total_cache_read_tokens: metrics.summary.total_cache_read_tokens,
            total_cost_usd: metrics.summary.total_cost_usd,
        },
        by_model: metrics
            .by_model
            .into_iter()
            .map(|m| UserModelMetrics {
                model_name: m.model_name,
                requests: m.requests,
                input_tokens: m.input_tokens,
                output_tokens: m.output_tokens,
                cache_read_tokens: m.cache_read_tokens,
                cost_usd: m.cost_usd,
            })
            .collect(),
        by_workspace: metrics
            .by_workspace
            .into_iter()
            .map(|w| UserWorkspaceMetrics {
                workspace_id: w.workspace_id.to_string(),
                workspace_name: w.workspace_name,
                requests: w.requests,
                input_tokens: w.input_tokens,
                output_tokens: w.output_tokens,
                cache_read_tokens: w.cache_read_tokens,
                cost_usd: w.cost_usd,
            })
            .collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/timeseries",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID"),
        ("start" = Option<String>, Query, description = "Start date (ISO 8601, default: 30 days ago)"),
        ("end" = Option<String>, Query, description = "End date (ISO 8601, default: now)"),
        ("granularity" = Option<String>, Query, description = "Time bucket size: hour, day, week (default: day)")
    ),
    responses(
        (status = 200, description = "Organization usage timeseries", body = UserTimeSeriesMetrics),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_user_organization_timeseries(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    Query(query): Query<TimeSeriesQuery>,
) -> Result<ResponseJson<UserTimeSeriesMetrics>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let organization_id = check_org_membership(&app_state, user, &org_id).await?;

    let granularity = query.granularity.as_deref().unwrap_or("day");
    if !["hour", "day", "week"].contains(&granularity) {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid granularity. Must be one of: hour, day, week.".to_string(),
                "invalid_granularity".to_string(),
            )),
        ));
    }

    let now = Utc::now();
    let end = parse_datetime_or_default(&query.end, now)?;
    let start = parse_datetime_or_default(&query.start, end - Duration::days(30))?;

    validate_date_range(start, end)?;

    let timeseries = app_state
        .analytics_service
        .get_organization_timeseries(organization_id, start, end, granularity)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get organization timeseries: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve organization timeseries".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok(ResponseJson(UserTimeSeriesMetrics {
        organization_id: timeseries.organization_id.to_string(),
        period_start: timeseries.period_start.to_rfc3339(),
        period_end: timeseries.period_end.to_rfc3339(),
        granularity: timeseries.granularity,
        data: timeseries
            .data
            .into_iter()
            .map(|p| UserTimeSeriesPoint {
                date: p.date,
                requests: p.requests,
                input_tokens: p.input_tokens,
                output_tokens: p.output_tokens,
                cache_read_tokens: p.cache_read_tokens,
                cost_usd: p.cost_usd,
            })
            .collect(),
    }))
}

/// Period selector for the by-model usage breakdown.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageByModelPeriod {
    Day,
    Week,
    Month,
}

impl UsageByModelPeriod {
    fn since(self) -> chrono::DateTime<Utc> {
        let now = Utc::now();
        match self {
            UsageByModelPeriod::Day => now - Duration::days(1),
            UsageByModelPeriod::Week => now - Duration::days(7),
            UsageByModelPeriod::Month => now - Duration::days(30),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            UsageByModelPeriod::Day => "day",
            UsageByModelPeriod::Week => "week",
            UsageByModelPeriod::Month => "month",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UsageByModelQuery {
    /// `day` (last 24h), `week` (last 7d), or `month` (last 30d). Defaults to `month`.
    #[serde(default = "default_period")]
    pub period: UsageByModelPeriod,
}

fn default_period() -> UsageByModelPeriod {
    UsageByModelPeriod::Month
}

/// Per-model usage aggregation entry
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageByModelEntryResponse {
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub total_cost: i64,            // In nano-dollars (scale 9)
    pub total_cost_display: String, // Human readable, e.g., "$0.00123"
    pub request_count: i64,
}

/// Per-model usage breakdown response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageByModelResponse {
    pub period: String,
    pub start_date: String,
    pub data: Vec<UsageByModelEntryResponse>,
}

/// Get organization usage broken down by model.
///
/// Returns one row per model, summed over a rolling window ending now:
/// `day` = last 24h, `week` = last 7 days, `month` = last 30 days (NOT calendar
/// day/week/month-to-date). Used by the dashboard pie chart to show which models
/// drive spend.
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/by-model",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID"),
        ("period" = Option<String>, Query, description = "Rolling window: `day` (last 24h), `week` (last 7d), or `month` (last 30d). Default: `month`")
    ),
    responses(
        (status = 200, description = "Per-model usage breakdown", body = UsageByModelResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_usage_by_model(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    Query(query): Query<UsageByModelQuery>,
) -> Result<ResponseJson<UsageByModelResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let organization_id = check_org_membership(&app_state, user, &org_id).await?;
    let start_date = query.period.since();

    let entries = app_state
        .usage_service
        .get_usage_by_model(organization_id, start_date)
        .await
        .map_err(|e| {
            tracing::error!(error = ?e, "Failed to get usage by model");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve usage breakdown".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    let data = entries
        .into_iter()
        .map(|e| UsageByModelEntryResponse {
            model: e.model,
            input_tokens: e.input_tokens,
            output_tokens: e.output_tokens,
            total_tokens: e.total_tokens,
            total_cost: e.total_cost,
            total_cost_display: format_amount(e.total_cost),
            request_count: e.request_count,
        })
        .collect();

    Ok(ResponseJson(UsageByModelResponse {
        period: query.period.as_str().to_string(),
        start_date: start_date.to_rfc3339(),
        data,
    }))
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
