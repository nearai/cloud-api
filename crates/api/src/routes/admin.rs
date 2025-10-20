use crate::middleware::AdminUser;
use crate::models::{
    AdminAccessTokenResponse, AdminUserResponse, BatchUpdateModelApiRequest,
    CreateAdminAccessTokenRequest, DecimalPrice, ErrorResponse, ListUsersResponse, ModelMetadata,
    ModelPricingHistoryEntry, ModelPricingHistoryResponse, ModelWithPricing, OrgLimitsHistoryEntry,
    OrgLimitsHistoryResponse, SpendLimit, UpdateOrganizationLimitsRequest,
    UpdateOrganizationLimitsResponse,
};
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use config::ApiConfig;
use services::admin::{AdminService, UpdateModelAdminRequest};
use services::auth::{AuthServiceTrait, UserId};
use std::sync::Arc;
use tracing::{debug, error};

#[derive(Clone)]
pub struct AdminAppState {
    pub admin_service: Arc<dyn AdminService + Send + Sync>,
    pub auth_service: Arc<dyn AuthServiceTrait>,
    pub config: Arc<ApiConfig>,
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
        ("session_token" = [])
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
                    public_name: request.public_name.clone(),
                    input_cost_per_token: request.input_cost_per_token.as_ref().map(|p| p.amount),
                    output_cost_per_token: request.output_cost_per_token.as_ref().map(|p| p.amount),
                    model_display_name: request.model_display_name.clone(),
                    model_description: request.model_description.clone(),
                    model_icon: request.model_icon.clone(),
                    context_length: request.context_length,
                    verifiable: request.verifiable,
                    is_active: request.is_active,
                    aliases: request.aliases.clone(),
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
                services::admin::AdminError::PublicNameConflict(msg) => (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(msg, "public_name_conflict".to_string())),
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
        .into_values()
        .map(|updated_model| ModelWithPricing {
            model_id: updated_model.public_name,
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
///
/// **Note:** Model names containing forward slashes (e.g., "Qwen/Qwen3-30B-A3B-Instruct-2507") must be URL-encoded.
/// For example, use "Qwen%2FQwen3-30B-A3B-Instruct-2507" in the URL path.
#[utoipa::path(
    get,
    path = "/admin/models/{model_name}/pricing-history",
    tag = "Admin",
    params(
        ("model_name" = String, Path, description = "Model name to get pricing history for (URL-encode if it contains slashes)"),
        ("limit" = i64, Query, description = "Maximum number of history entries to return (default: 50)"),
        ("offset" = i64, Query, description = "Number of history entries to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Pricing history retrieved successfully", body = ModelPricingHistoryResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_model_pricing_history(
    State(app_state): State<AdminAppState>,
    Path(model_name): Path<String>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<PricingHistoryQueryParams>,
) -> Result<ResponseJson<ModelPricingHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "Get pricing history request for model: {}, limit={}, offset={}",
        model_name, params.limit, params.offset
    );

    let (history, total) = app_state
        .admin_service
        .get_pricing_history(&model_name, params.limit, params.offset)
        .await
        .map_err(|e| {
            error!("Failed to get pricing history: {}", e);
            match e {
                services::admin::AdminError::ModelNotFound(_) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        format!("Model '{model_name}' not found"),
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
        total,
        limit: params.limit,
        offset: params.offset,
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
        ("session_token" = [])
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
        "Update organization limits request for org_id: {}, amount: {} nano-dollars, currency: {}",
        org_id, request.spend_limit.amount, request.spend_limit.currency
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
            scale: 9, // Always scale 9 (nano-dollars)
            currency: "USD".to_string(),
        },
        updated_at: updated_limits.effective_from.to_rfc3339(),
    };

    Ok(ResponseJson(response))
}

/// Get limits history for an organization (Admin only)
///
/// Returns the complete limits history for a specific organization, showing all limits changes over time.
/// Get limits history for an organization (Admin only)
///
/// Returns the complete limits history for a specific organization, showing all limits changes over time.
#[utoipa::path(
    get,
    path = "/admin/organizations/{organization_id}/limits/history",
    tag = "Admin",
    params(
        ("organization_id" = String, Path, description = "The organization's ID (as a UUID)"),
        ("limit" = i64, Query, description = "Maximum number of history records to return (default: 50)"),
        ("offset" = i64, Query, description = "Number of records to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Limits history retrieved successfully", body = OrgLimitsHistoryResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_limits_history(
    State(app_state): State<AdminAppState>,
    Path(organization_id): Path<String>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<OrgLimitsHistoryQueryParams>,
) -> Result<ResponseJson<OrgLimitsHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    let organization_uuid = match uuid::Uuid::parse_str(&organization_id) {
        Ok(id) => id,
        Err(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Invalid organization ID format".to_string(),
                    "invalid_request".to_string(),
                )),
            ));
        }
    };

    debug!(
        "Get limits history for organization_id={}, limit={}, offset={}",
        organization_id, params.limit, params.offset
    );

    let (history, total) = app_state
        .admin_service
        .get_organization_limits_history(organization_uuid, params.limit, params.offset)
        .await
        .map_err(|e| {
            error!("Failed to retrieve organization limits history: {}", e);
            match e {
                services::admin::AdminError::OrganizationNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        msg,
                        "organization_not_found".to_string(),
                    )),
                ),
                services::admin::AdminError::Unauthorized(msg) => (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve limits history".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let entries: Vec<OrgLimitsHistoryEntry> = history
        .into_iter()
        .map(|h| OrgLimitsHistoryEntry {
            id: h.id.to_string(),
            organization_id: h.organization_id.to_string(),
            spend_limit: SpendLimit {
                amount: h.spend_limit,
                scale: 9,
                currency: "USD".to_string(),
            },
            effective_from: h.effective_from.to_rfc3339(),
            effective_until: h.effective_until.map(|dt| dt.to_rfc3339()),
            changed_by: h.changed_by,
            change_reason: h.change_reason,
            created_at: h.created_at.to_rfc3339(),
        })
        .collect();

    let response = OrgLimitsHistoryResponse {
        history: entries,
        total,
        limit: params.limit,
        offset: params.offset,
    };

    Ok(ResponseJson(response))
}

/// Delete a model (Admin only)
///
/// Soft deletes a model by setting is_active to false. This preserves historical usage records
/// that reference the model name while preventing it from being used in new requests.
///
/// **Note:** Model names containing forward slashes (e.g., "Qwen/Qwen3-30B-A3B-Instruct-2507") must be URL-encoded.
/// For example, use "Qwen%2FQwen3-30B-A3B-Instruct-2507" in the URL path.
#[utoipa::path(
    delete,
    path = "/admin/models/{model_name}",
    tag = "Admin",
    params(
        ("model_name" = String, Path, description = "Model name to delete (URL-encode if it contains slashes)")
    ),
    responses(
        (status = 204, description = "Model deleted successfully"),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn delete_model(
    State(app_state): State<AdminAppState>,
    Path(model_name): Path<String>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<StatusCode, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Delete model request for: {}", model_name);

    app_state
        .admin_service
        .delete_model(&model_name)
        .await
        .map_err(|e| {
            error!("Failed to delete model: {}", e);
            match e {
                services::admin::AdminError::ModelNotFound(_) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        format!("Model '{model_name}' not found"),
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
                        "Failed to delete model".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    Ok(StatusCode::NO_CONTENT)
}

/// List all registered users with pagination (Admin only)
///
/// Returns a paginated list of all users in the system. Only authenticated admins can perform this operation.
#[utoipa::path(
    get,
    path = "/admin/users",
    tag = "Admin",
    params(
        ("limit" = i64, Query, description = "Maximum number of users to return (default: 50)"),
        ("offset" = i64, Query, description = "Number of users to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Users retrieved successfully", body = ListUsersResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_users(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<ListUsersQueryParams>,
) -> Result<ResponseJson<ListUsersResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "List users request with limit={}, offset={}",
        params.limit, params.offset
    );

    let (users, total) = app_state
        .admin_service
        .list_users(params.limit, params.offset)
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
        limit: params.limit,
        offset: params.offset,
    };

    Ok(ResponseJson(response))
}

/// Create admin access token (Admin only)
///
/// Creates an access token for admin users with customizable expiration time, IP address, and user agent.
/// This is typically used by billing services and other automated systems that need access to admin endpoints.
///
/// **Security Note:** These tokens can have very long expiration times and should be used with caution.
/// Store them securely and rotate them regularly.
#[utoipa::path(
    post,
    path = "/admin/access_token",
    tag = "Admin",
    request_body = CreateAdminAccessTokenRequest,
    responses(
        (status = 200, description = "Admin access token created successfully", body = AdminAccessTokenResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn create_admin_access_token(
    State(app_state): State<AdminAppState>,
    Extension(admin_user): Extension<AdminUser>, // Require admin auth
    Json(request): Json<CreateAdminAccessTokenRequest>,
) -> Result<ResponseJson<AdminAccessTokenResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Creating admin access token for user: {} with {} hours expiration",
        admin_user.0.email, request.expires_in_hours
    );

    // Validate expiration time (must be positive)
    if request.expires_in_hours <= 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "expires_in_hours must be a positive number".to_string(),
                "invalid_request".to_string(),
            )),
        ));
    }

    // Create session with the specified parameters
    let user_id = UserId(admin_user.0.id);
    let session_result = app_state
        .auth_service
        .create_session(
            user_id,
            request.ip_address,
            request.user_agent,
            app_state.config.auth.encoding_key.to_string(),
            request.expires_in_hours,
            0,
        )
        .await;

    match session_result {
        Ok((access_token, refresh_session, _refresh_token)) => {
            debug!(
                "Admin access token created successfully for user: {}",
                admin_user.0.email
            );

            let response = AdminAccessTokenResponse {
                access_token,
                created_by_user_id: admin_user.0.id.to_string(),
                created_at: refresh_session.created_at,
                message: format!(
                    "Admin access token created successfully. Token expires in {} hours and should be stored securely.",
                    request.expires_in_hours
                ),
            };

            Ok(ResponseJson(response))
        }
        Err(e) => {
            error!("Failed to create admin access token: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to create admin access token: {e}"),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ListUsersQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

#[derive(Debug, serde::Deserialize)]
pub struct PricingHistoryQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

#[derive(Debug, serde::Deserialize)]
pub struct OrgLimitsHistoryQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}
