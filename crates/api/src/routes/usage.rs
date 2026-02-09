use crate::{
    middleware::{auth::AuthenticatedApiKey, AuthenticatedUser},
    models::ErrorResponse,
    routes::{api::AppState, common::format_amount},
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

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

/// Usage history entry
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageHistoryEntryResponse {
    pub id: String,
    pub workspace_id: String,
    pub api_key_id: String,
    pub model: String, // Model name (canonical name from models table)
    pub input_tokens: i32,
    pub output_tokens: i32,
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

    let organization_id = Uuid::parse_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Check if user is a member of this organization
    let user_id = crate::conversions::authenticated_user_to_user_id(user);
    let is_member = app_state
        .organization_service
        .is_member(
            services::organization::OrganizationId(organization_id),
            user_id,
        )
        .await
        .map_err(|_| {
            tracing::error!("Failed to check organization membership");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to verify organization access".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    if !is_member {
        return Err((
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "You are not authorized to access this organization's usage balance.".to_string(),
                "forbidden".to_string(),
            )),
        ));
    }

    let balance = app_state
        .usage_service
        .get_balance(organization_id)
        .await
        .map_err(|_| {
            tracing::error!("Failed to get balance");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve balance".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    // Get spending limit
    let limit = app_state
        .usage_service
        .get_limit(organization_id)
        .await
        .map_err(|_| {
            tracing::error!("Failed to get limit");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve limit".to_string(),
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

            Ok(ResponseJson(OrganizationBalanceResponse {
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
            }))
        }
        None => {
            // No balance yet, but may have a limit
            if let Some(limit_info) = limit {
                // Organization has a limit but no usage yet - return with zero balance
                Ok(ResponseJson(OrganizationBalanceResponse {
                    organization_id: org_id,
                    total_spent: 0,
                    total_spent_display: format_amount(0),
                    spend_limit: Some(limit_info.spend_limit),
                    spend_limit_display: Some(format_amount(limit_info.spend_limit)),
                    remaining: Some(limit_info.spend_limit),
                    remaining_display: Some(format_amount(limit_info.spend_limit)),
                    last_usage_at: None,
                    total_requests: 0,
                    total_tokens: 0,
                    updated_at: chrono::Utc::now().to_rfc3339(),
                }))
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

    let organization_id = Uuid::parse_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Check if user is a member of this organization
    let user_id = crate::conversions::authenticated_user_to_user_id(user);
    let is_member = app_state
        .organization_service
        .is_member(
            services::organization::OrganizationId(organization_id),
            user_id,
        )
        .await
        .map_err(|_| {
            tracing::error!("Failed to check organization membership");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to verify organization access".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    if !is_member {
        return Err((
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "You are not authorized to access this organization's usage history.".to_string(),
                "forbidden".to_string(),
            )),
        ));
    }

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
// POST /v1/usage — Record usage
// ============================================

/// POST /v1/usage response body — tagged union matching the request type.
/// All costs use fixed scale of 9 (nano-dollars) and USD currency.
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

/// Record usage
///
/// Record a usage event. The server calculates costs based on the model's pricing.
/// Uses a tagged union on the `type` field to distinguish usage kinds.
///
/// A required `id` field serves as an **idempotency key**. It is stored as
/// `provider_request_id` and hashed to a deterministic UUID v5 for `inference_id`.
/// If a record with the same `id` already exists within the same organization,
/// the existing record is returned without double-charging.
///
/// ## Chat completion example
/// ```json
/// { "type": "chat_completion", "model": "Qwen/Qwen3-30B-A3B-Instruct-2507", "input_tokens": 100, "output_tokens": 50, "id": "req-abc-123" }
/// ```
///
/// ## Image generation example
/// ```json
/// { "type": "image_generation", "model": "black-forest-labs/FLUX.1", "image_count": 2, "id": "req-img-456" }
/// ```
#[utoipa::path(
    post,
    path = "/v1/usage",
    tag = "Usage",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Usage recorded successfully", body = RecordUsageResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 402, description = "Insufficient credits", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn record_usage(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Json(request): Json<services::usage::RecordUsageApiRequest>,
) -> Result<ResponseJson<RecordUsageResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let api_key_id = Uuid::parse_str(&api_key.api_key.id.0).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                "Invalid API key ID".to_string(),
                "internal_server_error".to_string(),
            )),
        )
    })?;

    // Remember the request variant before moving it into the service call
    let is_image = matches!(
        &request,
        services::usage::RecordUsageApiRequest::ImageGeneration { .. }
    );

    let entry = app_state
        .usage_service
        .record_usage_from_api(
            api_key.organization.id.0,
            api_key.workspace.id.0,
            api_key_id,
            request,
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
                tracing::error!("Failed to record usage");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to record usage".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    let response = if is_image {
        RecordUsageResponse::ImageGeneration {
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
        }
    } else {
        RecordUsageResponse::ChatCompletion {
            id: entry.id.to_string(),
            model: entry.model,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
            total_tokens: entry.total_tokens,
            input_cost: entry.input_cost,
            output_cost: entry.output_cost,
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            created_at: entry.created_at.to_rfc3339(),
        }
    };

    Ok(ResponseJson(response))
}
