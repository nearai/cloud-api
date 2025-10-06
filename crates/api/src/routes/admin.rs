use crate::middleware::AdminUser;
use crate::models::{
    AdminUserResponse, BatchUpdateModelApiRequest, DecimalPrice, ErrorResponse, ListUsersResponse,
    ModelMetadata, ModelPricingHistoryEntry, ModelPricingHistoryResponse, ModelWithPricing,
    SpendLimit, UpdateOrganizationLimitsRequest, UpdateOrganizationLimitsResponse,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use services::admin::{AdminService, UpdateModelAdminRequest};
use std::sync::Arc;
use tracing::{debug, error};

#[derive(Clone)]
pub struct AdminAppState {
    pub admin_service: Arc<dyn AdminService + Send + Sync>,
}

/// Batch upsert models metadata (Admin only)
///
/// Upserts (inserts or updates) pricing and metadata for one or more models. Only authenticated admins can perform this operation.
/// The body should be an array of objects where each key is a model name and the value is the model data.
#[utoipa::path(
    patch,
    path = "/admin/models",
    tag = "Admin",
    request_body = BatchUpdateModelApiRequest,
    responses(
        (status = 200, description = "Models upserted successfully", body = Vec<ModelWithPricing>),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = [])
    )
)]
pub async fn batch_upsert_models(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    ResponseJson(batch_request): ResponseJson<BatchUpdateModelApiRequest>,
) -> Result<ResponseJson<Vec<ModelWithPricing>>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Batch upsert models request with {} model(s)",
        batch_request.len()
    );

    // Validate the batch request format
    if batch_request.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Request body must contain at least one model update".to_string(),
                "invalid_request".to_string(),
            )),
        ));
    }

    // Convert API request to service request
    let models = batch_request
        .iter()
        .map(|(model_name, request)| {
            (
                model_name.clone(),
                UpdateModelAdminRequest {
                    input_cost_per_token: request.input_cost_per_token.as_ref().map(|p| p.amount),
                    output_cost_per_token: request.output_cost_per_token.as_ref().map(|p| p.amount),
                    model_display_name: request.model_display_name.clone(),
                    model_description: request.model_description.clone(),
                    model_icon: request.model_icon.clone(),
                    context_length: request.context_length,
                    verifiable: request.verifiable,
                    is_active: request.is_active,
                },
            )
        })
        .collect();

    let updated_models = app_state
        .admin_service
        .batch_upsert_models(models)
        .await
        .map_err(|e| {
            error!("Failed to upsert models: {}", e);
            match e {
                services::admin::AdminError::ModelNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(msg, "model_not_found".to_string())),
                ),
                services::admin::AdminError::InvalidPricing(msg) => (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(msg, "invalid_pricing".to_string())),
                ),
                services::admin::AdminError::Unauthorized(msg) => (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to upsert models".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    // Convert to API response - map from HashMap to Vec
    let api_models: Vec<ModelWithPricing> = updated_models
        .into_iter()
        .map(|(model_name, updated_model)| ModelWithPricing {
            model_id: model_name,
            input_cost_per_token: DecimalPrice {
                amount: updated_model.input_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            output_cost_per_token: DecimalPrice {
                amount: updated_model.output_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            metadata: ModelMetadata {
                verifiable: updated_model.verifiable,
                context_length: updated_model.context_length,
                model_display_name: updated_model.model_display_name,
                model_description: updated_model.model_description,
                model_icon: updated_model.model_icon,
            },
        })
        .collect();

    Ok(ResponseJson(api_models))
}

/// Get pricing history for a model (Admin only)
///
/// Returns the complete pricing history for a specific model, showing all pricing changes over time.
#[utoipa::path(
    get,
    path = "/admin/models/{model_name}/pricing-history",
    tag = "Admin",
    params(
        ("model_name" = String, Path, description = "Model name to get pricing history for")
    ),
    responses(
        (status = 200, description = "Pricing history retrieved successfully", body = ModelPricingHistoryResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = [])
    )
)]
pub async fn get_model_pricing_history(
    State(app_state): State<AdminAppState>,
    Path(model_name): Path<String>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
) -> Result<ResponseJson<ModelPricingHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Get pricing history request for model: {}", model_name);

    let history = app_state
        .admin_service
        .get_pricing_history(&model_name)
        .await
        .map_err(|e| {
            error!("Failed to get pricing history: {}", e);
            match e {
                services::admin::AdminError::ModelNotFound(_) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        format!("Model '{}' not found", model_name),
                        "model_not_found".to_string(),
                    )),
                ),
                services::admin::AdminError::InvalidPricing(msg) => (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(msg, "invalid_request".to_string())),
                ),
                services::admin::AdminError::Unauthorized(msg) => (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve pricing history".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let history_entries: Vec<ModelPricingHistoryEntry> = history
        .into_iter()
        .map(|h| ModelPricingHistoryEntry {
            id: h.id.to_string(),
            model_id: h.model_id.to_string(),
            input_cost_per_token: DecimalPrice {
                amount: h.input_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            output_cost_per_token: DecimalPrice {
                amount: h.output_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            context_length: h.context_length,
            model_display_name: h.model_display_name,
            model_description: h.model_description,
            effective_from: h.effective_from.to_rfc3339(),
            effective_until: h.effective_until.map(|dt| dt.to_rfc3339()),
            changed_by: h.changed_by,
            change_reason: h.change_reason,
            created_at: h.created_at.to_rfc3339(),
        })
        .collect();

    let response = ModelPricingHistoryResponse {
        model_name,
        history: history_entries,
    };

    Ok(ResponseJson(response))
}

/// Update organization limits (Admin only)
///
/// Updates spending limits for a specific organization. This endpoint is typically called by
/// a billing service with an admin API key when a customer makes a purchase.
#[utoipa::path(
    patch,
    path = "/admin/organizations/{org_id}/limits",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID to update limits for")
    ),
    request_body = UpdateOrganizationLimitsRequest,
    responses(
        (status = 200, description = "Organization limits updated successfully", body = UpdateOrganizationLimitsResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = [])
    )
)]
pub async fn update_organization_limits(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    ResponseJson(request): ResponseJson<UpdateOrganizationLimitsRequest>,
) -> Result<ResponseJson<UpdateOrganizationLimitsResponse>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!(
        "Update organization limits request for org_id: {}, amount: {}, scale: {}, currency: {}",
        org_id, request.spend_limit.amount, request.spend_limit.scale, request.spend_limit.currency
    );

    // Parse organization ID
    let organization_id = uuid::Uuid::parse_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID format".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Convert API request to service request
    let service_request = services::admin::OrganizationLimitsUpdate {
        spend_limit: request.spend_limit.amount,
        changed_by: request.changed_by,
        change_reason: request.change_reason,
    };

    // Update organization limits via admin service
    let updated_limits = app_state
        .admin_service
        .update_organization_limits(organization_id, service_request)
        .await
        .map_err(|e| {
            error!("Failed to update organization limits: {}", e);
            match e {
                services::admin::AdminError::OrganizationNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        msg,
                        "organization_not_found".to_string(),
                    )),
                ),
                services::admin::AdminError::InvalidLimits(msg) => (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(msg, "invalid_limits".to_string())),
                ),
                services::admin::AdminError::Unauthorized(msg) => (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to update organization limits".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    // Convert service response to API response
    let response = UpdateOrganizationLimitsResponse {
        organization_id: updated_limits.organization_id.to_string(),
        spend_limit: SpendLimit {
            amount: updated_limits.spend_limit,
            scale: 9,
            currency: "USD".to_string(),
        },
        updated_at: updated_limits.effective_from.to_rfc3339(),
    };

    Ok(ResponseJson(response))
}

/// List all registered users with pagination (Admin only)
///
/// Returns a paginated list of all users in the system. Only authenticated admins can perform this operation.
#[utoipa::path(
    get,
    path = "/admin/users",
    tag = "Admin",
    params(
        ("limit" = Option<i64>, Query, description = "Maximum number of users to return (default: 50)"),
        ("offset" = Option<i64>, Query, description = "Number of users to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Users retrieved successfully", body = ListUsersResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = [])
    )
)]
pub async fn list_users(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<ListUsersQueryParams>,
) -> Result<ResponseJson<ListUsersResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let limit = params.limit.unwrap_or(50).min(100); // Cap at 100 for safety
    let offset = params.offset.unwrap_or(0);

    debug!("List users request with limit={}, offset={}", limit, offset);

    let (users, total) = app_state
        .admin_service
        .list_users(limit, offset)
        .await
        .map_err(|e| {
            error!("Failed to list users: {}", e);
            match e {
                services::admin::AdminError::Unauthorized(msg) => (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve users".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let user_responses: Vec<AdminUserResponse> = users
        .into_iter()
        .map(|u| AdminUserResponse {
            id: u.id.to_string(),
            email: u.email,
            username: Some(u.username),
            display_name: u.display_name,
            avatar_url: u.avatar_url,
            created_at: u.created_at,
            last_login_at: u.last_login_at,
            is_active: u.is_active,
        })
        .collect();

    let response = ListUsersResponse {
        users: user_responses,
        total,
        limit,
        offset,
    };

    Ok(ResponseJson(response))
}

#[derive(Debug, serde::Deserialize)]
pub struct ListUsersQueryParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}
