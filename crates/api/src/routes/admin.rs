use crate::middleware::AdminUser;
use crate::models::{
    AdminAccessTokenResponse, AdminUserOrganizationDetails, AdminUserResponse,
    BatchUpdateModelApiRequest, CreateAdminAccessTokenRequest, DecimalPrice,
    DeleteAdminAccessTokenRequest, ErrorResponse, ListUsersResponse, ModelHistoryEntry,
    ModelHistoryResponse, ModelMetadata, ModelWithPricing, OrgLimitsHistoryEntry,
    OrgLimitsHistoryResponse, SpendLimit, UpdateOrganizationLimitsRequest,
    UpdateOrganizationLimitsResponse,
};
use axum::{
    extract::{Json, Path, Query, State},
    http::HeaderMap,
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use chrono::{Duration, Utc};
use config::ApiConfig;
use services::admin::{AdminService, AnalyticsService, UpdateModelAdminRequest};
use services::auth::AuthServiceTrait;
use std::sync::Arc;
use tracing::{debug, error};

#[derive(Clone)]
pub struct AdminAppState {
    pub admin_service: Arc<dyn AdminService + Send + Sync>,
    pub analytics_service: Arc<AnalyticsService>,
    pub auth_service: Arc<dyn AuthServiceTrait>,
    pub config: Arc<ApiConfig>,
    pub admin_access_token_repository: Arc<database::repositories::AdminAccessTokenRepository>,
}

/// Batch upsert models metadata (Admin only)
///
/// Upserts (inserts or updates) pricing and metadata for one or more models. Only authenticated admins can perform this operation.
/// The body should be an array of objects where each key is a model name and the value is the model data.
#[utoipa::path(
    patch,
    path = "/v1/admin/models",
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
            error!("Failed to upsert models");
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
                        format!("Failed to upsert models, error: {e:?}"),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    // Convert to API response - map from HashMap to Vec
    // The key in the HashMap is the canonical model_name
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
                aliases: updated_model.aliases,
            },
        })
        .collect();

    Ok(ResponseJson(api_models))
}

/// Get complete history for a model (Admin only)
///
/// Returns the complete history for a specific model, showing all changes over time including pricing,
/// context length, display name, and description.
///
/// **Note:** Model names containing forward slashes (e.g., "Qwen/Qwen3-30B-A3B-Instruct-2507") must be URL-encoded.
/// For example, use "Qwen%2FQwen3-30B-A3B-Instruct-2507" in the URL path.
#[utoipa::path(
    get,
    path = "/v1/admin/models/{model_name}/history",
    tag = "Admin",
    params(
        ("model_name" = String, Path, description = "Model name to get complete history for (URL-encode if it contains slashes)"),
        ("limit" = Option<i64>, Query, description = "Maximum number of history entries to return (default: 50)"),
        ("offset" = Option<i64>, Query, description = "Number of history entries to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Model history retrieved successfully", body = ModelHistoryResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_model_history(
    State(app_state): State<AdminAppState>,
    Path(model_name): Path<String>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<ModelHistoryQueryParams>,
) -> Result<ResponseJson<ModelHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "Get model history request for model: {}, limit={}, offset={}",
        model_name, params.limit, params.offset
    );

    let (history, total) = app_state
        .admin_service
        .get_model_history(&model_name, params.limit, params.offset)
        .await
        .map_err(|e| {
            error!("Failed to get model history");
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
                        "Failed to retrieve model history".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let history_entries: Vec<ModelHistoryEntry> = history
        .into_iter()
        .map(|h| ModelHistoryEntry {
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

    let response = ModelHistoryResponse {
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
    path = "/v1/admin/organizations/{org_id}/limits",
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
    Extension(admin_user): Extension<AdminUser>, // Require admin auth
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

    // Extract admin user ID and email from authenticated user
    let admin_user_id = admin_user.0.id;
    let admin_user_email = admin_user.0.email.clone();

    // Convert API request to service request
    let service_request = services::admin::OrganizationLimitsUpdate {
        spend_limit: request.spend_limit.amount,
        changed_by: request.changed_by,
        change_reason: request.change_reason,
        changed_by_user_id: Some(admin_user_id),
        changed_by_user_email: Some(admin_user_email),
    };

    // Update organization limits via admin service
    let updated_limits = app_state
        .admin_service
        .update_organization_limits(organization_id, service_request)
        .await
        .map_err(|e| {
            error!("Failed to update organization limits");
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
    path = "/v1/admin/organizations/{organization_id}/limits/history",
    tag = "Admin",
    params(
        ("organization_id" = String, Path, description = "The organization's ID (as a UUID)"),
        ("limit" = Option<i64>, Query, description = "Maximum number of history records to return (default: 50)"),
        ("offset" = Option<i64>, Query, description = "Number of records to skip (default: 0)")
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
            error!("Failed to retrieve organization limits history");
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
            changed_by_user_id: h.changed_by_user_id.map(|id| id.to_string()),
            changed_by_user_email: h.changed_by_user_email,
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
    path = "/v1/admin/models/{model_name}",
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
            error!("Failed to delete model");
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
    path = "/v1/admin/users",
    tag = "Admin",
    params(
        ("limit" = Option<i64>, Query, description = "Maximum number of users to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Number of users to skip (default: 0)"),
        ("include_organizations" = Option<bool>, Query, description = "Whether to include organization information and spend limits for the first organization owned by each user (default: false)")
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
        "List users request with limit={}, offset={}, include_organizations={}",
        params.limit, params.offset, params.include_organizations
    );

    let (user_responses, total) = if params.include_organizations {
        // Fetch users with their default organization and spend limit
        let (users_with_orgs, total) = app_state
            .admin_service
            .list_users_with_organizations(params.limit, params.offset)
            .await
            .map_err(|e| {
                error!("Failed to list users with organizations");
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

        let responses: Vec<AdminUserResponse> = users_with_orgs
            .into_iter()
            .map(|(u, org_data)| {
                let organizations = org_data.map(|org_info| {
                    vec![AdminUserOrganizationDetails {
                        id: org_info.id.to_string(),
                        name: org_info.name,
                        description: org_info.description,
                        spend_limit: org_info.spend_limit.map(|amount| SpendLimit {
                            amount,
                            scale: 9,
                            currency: "USD".to_string(),
                        }),
                    }]
                });

                AdminUserResponse {
                    id: u.id.to_string(),
                    email: u.email,
                    username: Some(u.username),
                    display_name: u.display_name,
                    avatar_url: u.avatar_url,
                    created_at: u.created_at,
                    last_login_at: u.last_login_at,
                    is_active: u.is_active,
                    organizations,
                }
            })
            .collect();

        (responses, total)
    } else {
        // Return users data only
        let (users, total) = app_state
            .admin_service
            .list_users(params.limit, params.offset)
            .await
            .map_err(|e| {
                error!("Failed to list users");
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

        let responses: Vec<AdminUserResponse> = users
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
                organizations: None,
            })
            .collect();

        (responses, total)
    };

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
    path = "/v1/admin/access-tokens",
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
    headers: HeaderMap,
    Json(request_body): Json<CreateAdminAccessTokenRequest>,
) -> Result<ResponseJson<AdminAccessTokenResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let user_agent = headers
        .get("User-Agent")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    debug!(
        "Creating admin access token for user: {} with {} hours expiration; (User-Agent: {:?})",
        admin_user.0.email, request_body.expires_in_hours, user_agent
    );

    // Validate expiration time (must be positive)
    if request_body.expires_in_hours <= 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "expires_in_hours must be a positive number".to_string(),
                "invalid_request".to_string(),
            )),
        ));
    }

    // Create admin access token directly in database
    let expires_at = Utc::now() + chrono::Duration::hours(request_body.expires_in_hours);

    match app_state
        .admin_access_token_repository
        .create(
            admin_user.0.id,
            request_body.name,
            request_body.reason,
            expires_at,
            user_agent,
        )
        .await
    {
        Ok((admin_token, access_token)) => {
            debug!(
                "Admin access token created successfully for user: {}",
                admin_user.0.email
            );

            let response = AdminAccessTokenResponse {
                id: admin_token.id.to_string(),
                access_token,
                created_by_user_id: admin_user.0.id.to_string(),
                created_at: admin_token.created_at,
                expires_at: admin_token.expires_at,
                name: admin_token.name,
                reason: admin_token.creation_reason,
            };

            Ok(ResponseJson(response))
        }
        Err(e) => {
            error!("Failed to create admin access token");
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

/// List admin access tokens (Admin only)
///
/// Retrieves a paginated list of all admin access tokens in the system.
/// Only authenticated admins can access this endpoint.
#[utoipa::path(
    get,
    path = "/v1/admin/access-tokens",
    tag = "Admin",
    params(
        ("limit" = Option<i64>, Query, description = "Number of records to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Number of records to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Admin access tokens retrieved successfully"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_admin_access_tokens(
    State(app_state): State<AdminAppState>,
    Extension(admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<ListUsersQueryParams>,
) -> Result<ResponseJson<serde_json::Value>, (StatusCode, ResponseJson<ErrorResponse>)> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "List admin access tokens request with limit={}, offset={} by admin: {}",
        params.limit, params.offset, admin_user.0.email
    );

    match app_state
        .admin_access_token_repository
        .list(params.limit, params.offset)
        .await
    {
        Ok(tokens) => {
            let total = app_state
                .admin_access_token_repository
                .count()
                .await
                .unwrap_or(0);

            let response = serde_json::json!({
                "data": tokens,
                "limit": params.limit,
                "offset": params.offset,
                "total": total
            });

            Ok(ResponseJson(response))
        }
        Err(e) => {
            error!("Failed to list admin access tokens");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to list admin access tokens: {e}"),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Delete admin access token (Admin only)
///
/// Revokes an admin access token by setting it as inactive.
/// Only authenticated admins can perform this operation.
#[utoipa::path(
    delete,
    path = "/v1/admin/access-tokens/{token_id}",
    tag = "Admin",
    request_body = DeleteAdminAccessTokenRequest,
    params(
        ("token_id" = String, Path, description = "ID of the admin access token to revoke")
    ),
    responses(
        (status = 200, description = "Admin access token revoked successfully"),
        (status = 404, description = "Admin access token not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn delete_admin_access_token(
    State(app_state): State<AdminAppState>,
    Path(token_id): Path<String>,
    Extension(admin_user): Extension<AdminUser>, // Require admin auth
    Json(request): Json<DeleteAdminAccessTokenRequest>,
) -> Result<ResponseJson<serde_json::Value>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Delete admin access token request for token_id: {} by admin: {}",
        token_id, admin_user.0.email
    );

    // Parse token ID
    let token_uuid = uuid::Uuid::parse_str(&token_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid token ID format".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Revoke the token
    match app_state
        .admin_access_token_repository
        .revoke(token_uuid, admin_user.0.id, request.reason)
        .await
    {
        Ok(true) => {
            debug!(
                "Admin access token {} revoked successfully by admin: {}",
                token_id, admin_user.0.email
            );

            let response = serde_json::json!({
                "message": "Admin access token revoked successfully",
                "token_id": token_id,
                "revoked_by": admin_user.0.email,
                "revoked_at": chrono::Utc::now().to_rfc3339()
            });

            Ok(ResponseJson(response))
        }
        Ok(false) => {
            debug!(
                "Admin access token {} not found or already revoked",
                token_id
            );
            Err((
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    "Admin access token not found or already revoked".to_string(),
                    "token_not_found".to_string(),
                )),
            ))
        }
        Err(e) => {
            error!("Failed to revoke admin access token");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to revoke admin access token: {e}"),
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
    #[serde(default)]
    pub include_organizations: bool,
}

#[derive(Debug, serde::Deserialize)]
pub struct ModelHistoryQueryParams {
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

#[derive(Debug, serde::Deserialize)]
pub struct MetricsQueryParams {
    /// Start of time range (ISO 8601 format). Defaults to 30 days ago.
    pub start: Option<String>,
    /// End of time range (ISO 8601 format). Defaults to now.
    pub end: Option<String>,
}

/// Get organization metrics for enterprise dashboards (Admin only)
///
/// Returns comprehensive usage metrics for an organization including:
/// - Summary totals (requests, tokens, cost)
/// - Breakdown by workspace
/// - Breakdown by API key
/// - Breakdown by model
///
/// This endpoint uses database queries instead of metrics services to provide
/// high-cardinality data (per-org, per-workspace, per-key) without the cost
/// of storing all combinations in Datadog/OTLP.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/metrics",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID to get metrics for"),
        ("start" = Option<String>, Query, description = "Start of time range (ISO 8601). Defaults to 30 days ago."),
        ("end" = Option<String>, Query, description = "End of time range (ISO 8601). Defaults to now.")
    ),
    responses(
        (status = 200, description = "Organization metrics retrieved successfully"),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_metrics(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Query(params): Query<MetricsQueryParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<ResponseJson<services::admin::OrganizationMetrics>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!(
        "Get organization metrics request for org_id: {}, start: {:?}, end: {:?}",
        org_id, params.start, params.end
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

    // Parse time range with defaults
    let end = params
        .end
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    let start = params
        .start
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| end - Duration::days(30));

    // Get metrics from analytics service
    let metrics = app_state
        .analytics_service
        .get_organization_metrics(organization_id, start, end)
        .await
        .map_err(|e| {
            error!("Failed to get organization metrics");
            match e {
                services::admin::AdminError::OrganizationNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        msg,
                        "organization_not_found".to_string(),
                    )),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        format!("Failed to retrieve metrics: {e}"),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    Ok(ResponseJson(metrics))
}
