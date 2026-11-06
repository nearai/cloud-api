use crate::conversions::{
    api_invitation_email_status_to_services, api_invitation_status_to_services,
    api_role_to_services_role, services_invitation_email_delivery_to_api,
    services_invitation_resend_result_to_api, services_member_to_api_member,
};
use crate::middleware::{AdminAudit, AdminUser};
use crate::models::{
    AdminAccessTokenPermission, AdminAccessTokenResponse, AdminInvitationEmailResendResultResponse,
    CreateAdminAccessTokenRequest, DeleteAdminAccessTokenRequest, EraseUserRequest,
    EraseUserResponse, ErasedOrganizationResponse, ErasureBlockedResponse, ErasureLogResponse,
    ErasureLookupOrganizationResponse, ErasurePreviewResponse, ErrorResponse,
    ListAdminAccessTokensResponse, ListAdminInvitationEmailDeliveriesResponse,
    LookupUserErasuresRequest, LookupUserErasuresResponse, OrganizationMemberResponse,
    RetainedOrganizationResponse, UpdateOrganizationMemberRequest, UserErasureRecordResponse,
};
use axum::{
    extract::{Json, Path, State},
    http::HeaderMap,
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use chrono::{DateTime, Utc};
use config::ApiConfig;
use services::admin::AdminService;
use services::auth::{AuthServiceTrait, UserId};
use std::sync::Arc;
use tracing::{debug, error};
use uuid::Uuid;

/// Serialize a stored `deprecation_date` into the OpenRouter-compatible
/// UTC-hour form `YYYY-MM-DDTHH:00:00Z` (matching the spec's examples, e.g.
/// `2025-06-01T15:00:00Z`). The stored value is always normalized to a whole
/// UTC hour by [`parse_deprecation_date`], so this is a faithful round-trip.
pub(crate) fn format_deprecation_date(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:00:00Z").to_string()
}

#[derive(Clone)]
pub struct AdminAppState {
    pub admin_service: Arc<dyn AdminService + Send + Sync>,
    pub organization_service:
        Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>,
    pub auth_service: Arc<dyn AuthServiceTrait>,
    pub config: Arc<ApiConfig>,
    pub admin_access_token_repository: Arc<database::repositories::AdminAccessTokenRepository>,
    pub infra_service: Arc<services::admin::InfraService>,
    pub user_erasure_service: Arc<services::user_erasure::UserErasureService>,
}

/// Update an organization member role as a system administrator.
///
/// Assigning the owner role transfers ownership from the current owner to an
/// existing organization member. The previous owner is demoted to admin.
#[utoipa::path(
    put,
    path = "/v1/admin/organizations/{org_id}/members/{user_id}",
    tag = "Admin",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("user_id" = Uuid, Path, description = "User ID")
    ),
    request_body = UpdateOrganizationMemberRequest,
    responses(
        (status = 200, description = "Member role updated", body = OrganizationMemberResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "System administrator privileges required", body = ErrorResponse),
        (status = 404, description = "Organization or member not found", body = ErrorResponse),
        (status = 422, description = "Malformed request body", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn update_organization_member_role(
    State(app_state): State<AdminAppState>,
    Extension(admin_user): Extension<AdminUser>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
    Json(request): Json<UpdateOrganizationMemberRequest>,
) -> Result<ResponseJson<OrganizationMemberResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let new_role = api_role_to_services_role(request.role);
    let update = app_state
        .organization_service
        .update_member_role_for_admin(
            services::organization::OrganizationId(org_id),
            UserId(user_id),
            new_role,
            UserId(admin_user.0.id),
        )
        .await
        .map_err(|error| match error {
            services::organization::OrganizationError::NotFound => (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    "Organization or member not found".to_string(),
                    "not_found".to_string(),
                )),
            ),
            services::organization::OrganizationError::InvalidParams(message) => (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(message, "invalid_request".to_string())),
            ),
            services::organization::OrganizationError::Unauthorized(message) => (
                StatusCode::FORBIDDEN,
                ResponseJson(ErrorResponse::new(message, "forbidden".to_string())),
            ),
            error => {
                error!("Failed to update organization member role: {:?}", error);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to update organization member role".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    tracing::info!(
        organization_id = %org_id,
        member_user_id = %user_id,
        admin_user_id = %admin_user.0.id,
        "System administrator updated organization member role"
    );

    Ok(ResponseJson(services_member_to_api_member(update.member)))
}

/// List organization invitation email deliveries (Admin only)
///
/// Returns delivery metadata for organization invitation emails without exposing invitation tokens.
#[utoipa::path(
    get,
    path = "/v1/admin/invitation-email-deliveries",
    tag = "Admin",
    params(
        ("limit" = Option<i64>, Query, description = "Maximum number of deliveries to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Number of deliveries to skip (default: 0)"),
        ("organization_id" = Option<Uuid>, Query, description = "Filter by organization ID"),
        ("recipient_email" = Option<String>, Query, description = "Case-insensitive recipient email substring filter"),
        ("email_status" = Option<crate::models::InvitationEmailStatus>, Query, description = "Filter by email delivery status"),
        ("invitation_status" = Option<crate::models::InvitationStatus>, Query, description = "Filter by invitation status"),
        ("created_after" = Option<DateTime<Utc>>, Query, description = "Only invitations created at or after this timestamp"),
        ("created_before" = Option<DateTime<Utc>>, Query, description = "Only invitations created at or before this timestamp")
    ),
    responses(
        (status = 200, description = "Invitation email deliveries retrieved successfully", body = ListAdminInvitationEmailDeliveriesResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_invitation_email_deliveries(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    audit: AdminAudit,
    axum::extract::Query(params): axum::extract::Query<ListInvitationEmailDeliveriesQueryParams>,
) -> Result<
    ResponseJson<ListAdminInvitationEmailDeliveriesResponse>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "List invitation email deliveries request with limit={}, offset={}",
        params.limit, params.offset
    );
    if let Some(organization_id) = params.organization_id {
        audit.record_organization(organization_id);
    }

    let filters = services::organization::InvitationEmailDeliveryFilters {
        organization_id: params
            .organization_id
            .map(services::organization::OrganizationId),
        recipient_email: params.recipient_email,
        email_status: params
            .email_status
            .map(api_invitation_email_status_to_services),
        invitation_status: params
            .invitation_status
            .map(api_invitation_status_to_services),
        created_after: params.created_after,
        created_before: params.created_before,
    };

    let (deliveries, total) = app_state
        .organization_service
        .list_invitation_email_deliveries(filters, params.limit, params.offset)
        .await
        .map_err(|e| match e {
            services::organization::OrganizationError::InvalidParams(msg) => (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(msg, "invalid_request".to_string())),
            ),
            _ => {
                error!("Failed to list invitation email deliveries: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve invitation email deliveries".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;
    audit.record_page(params.limit, params.offset, deliveries.len());

    let response = ListAdminInvitationEmailDeliveriesResponse {
        deliveries: deliveries
            .into_iter()
            .map(services_invitation_email_delivery_to_api)
            .collect(),
        total,
        limit: params.limit,
        offset: params.offset,
    };

    Ok(ResponseJson(response))
}

/// Resend a single organization invitation email (Admin only)
#[utoipa::path(
    post,
    path = "/v1/admin/invitation-email-deliveries/{invitation_id}/resend",
    tag = "Admin",
    params(
        ("invitation_id" = Uuid, Path, description = "Invitation ID")
    ),
    responses(
        (status = 200, description = "Invitation email resend attempted", body = AdminInvitationEmailResendResultResponse),
        (status = 400, description = "Invitation is not pending or has expired", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Invitation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn resend_invitation_email(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    Path(invitation_id): Path<Uuid>,
) -> Result<
    ResponseJson<AdminInvitationEmailResendResultResponse>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    debug!("Resend invitation email request for {}", invitation_id);

    app_state
        .organization_service
        .resend_invitation_email(invitation_id)
        .await
        .map(services_invitation_resend_result_to_api)
        .map(ResponseJson)
        .map_err(|e| match e {
            services::organization::OrganizationError::NotFound => (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    "Invitation not found".to_string(),
                    "not_found".to_string(),
                )),
            ),
            services::organization::OrganizationError::InvalidParams(msg) => (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(msg, "invalid_request".to_string())),
            ),
            _ => {
                error!("Failed to resend invitation email: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to resend invitation email".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })
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
        (status = 503, description = "Read-only token issuance is disabled during rollout", body = ErrorResponse),
        (status = 422, description = "Request deserialization failed (including invalid permission values)"),
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

    let expires_in_hours = request_body.expires_in_hours;
    let user_agent_present = user_agent.is_some();
    debug!(
        admin_user_id = %admin_user.0.id,
        expires_in_hours,
        user_agent_present,
        "Creating admin access token"
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

    if request_body.permission == AdminAccessTokenPermission::ReadOnly
        && !app_state.config.auth.admin_read_only_tokens_enabled
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            ResponseJson(ErrorResponse::new(
                "Read-only admin token issuance is not enabled".to_string(),
                "read_only_token_issuance_disabled".to_string(),
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
            request_body.permission.into(),
        )
        .await
    {
        Ok((admin_token, access_token)) => {
            debug!(
                admin_user_id = %admin_user.0.id,
                token_id = %admin_token.id,
                "Admin access token created successfully"
            );

            let response = AdminAccessTokenResponse {
                id: admin_token.id.to_string(),
                access_token,
                created_by_user_id: admin_user.0.id.to_string(),
                created_at: admin_token.created_at,
                expires_at: admin_token.expires_at,
                name: admin_token.name,
                reason: admin_token.creation_reason,
                permission: admin_token.permission.into(),
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
        (status = 200, description = "Admin access tokens retrieved successfully", body = ListAdminAccessTokensResponse),
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
    audit: AdminAudit,
    axum::extract::Query(params): axum::extract::Query<ListAdminAccessTokensQueryParams>,
) -> Result<ResponseJson<ListAdminAccessTokensResponse>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        admin_user_id = %admin_user.0.id,
        limit = params.limit,
        offset = params.offset,
        "List admin access tokens request"
    );

    match app_state
        .admin_access_token_repository
        .list(params.limit, params.offset)
        .await
    {
        Ok(tokens) => {
            audit.record_page(params.limit, params.offset, tokens.len());
            let total = app_state
                .admin_access_token_repository
                .count()
                .await
                .unwrap_or(0);

            let response = ListAdminAccessTokensResponse {
                data: tokens.into_iter().map(Into::into).collect(),
                limit: params.limit,
                offset: params.offset,
                total,
            };

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
        admin_user_id = %admin_user.0.id,
        token_id = %token_id,
        "Delete admin access token request"
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
                admin_user_id = %admin_user.0.id,
                token_id = %token_id,
                "Admin access token revoked successfully"
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
pub struct ListInvitationEmailDeliveriesQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    pub organization_id: Option<Uuid>,
    pub recipient_email: Option<String>,
    pub email_status: Option<crate::models::InvitationEmailStatus>,
    pub invitation_status: Option<crate::models::InvitationStatus>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ListAdminAccessTokensQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// Get the platform infrastructure / fleet burn summary (Admin only)
///
/// Fetches the live host list, counts active/idle hosts, and computes the monthly/daily
/// GPU burn rate from the configured cost-per-host. Degrades gracefully (stale=true) if
/// the host inventory is unreachable.
#[utoipa::path(
    get,
    path = "/v1/admin/platform/infra-summary",
    tag = "Admin",
    responses(
        (status = 200, description = "Infra summary retrieved successfully", body = services::admin::InfraSummary),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_infra_summary(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<ResponseJson<services::admin::InfraSummary>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!("Get platform infra summary request");
    let summary = app_state.infra_service.get_infra_summary().await;
    Ok(ResponseJson(summary))
}

fn repository_error_kind(error: &anyhow::Error) -> &'static str {
    use services::common::RepositoryError as R;
    let Some(repo_error) = error.chain().find_map(|e| e.downcast_ref::<R>()) else {
        return "other";
    };
    match repo_error {
        R::NotFound(_) => "not_found",
        R::AlreadyExists => "already_exists",
        R::RequiredFieldMissing(_) => "required_field_missing",
        R::ForeignKeyViolation(_) => "foreign_key_violation",
        R::ValidationFailed(_) => "validation_failed",
        R::DependencyExists(_) => "dependency_exists",
        R::TransactionConflict => "transaction_conflict",
        R::ConnectionFailed(_) => "connection_failed",
        R::AuthenticationFailed => "authentication_failed",
        R::QueryTimeout => "query_timeout",
        R::PoolError(_) => "pool",
        R::DatabaseError(_) => "database",
        R::DataConversionError(_) => "data_conversion",
    }
}

fn erasure_error_to_response(
    user_id: Option<uuid::Uuid>,
    err: services::user_erasure::UserErasureError,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use services::user_erasure::UserErasureError as E;
    let (status, error_type, message) = match err {
        E::NotFound => (StatusCode::NOT_FOUND, "not_found", "User not found"),
        E::Blocked(blockers) => {
            let ErrorResponse { error } =
                ErrorResponse::new("Erasure blocked".to_string(), "erasure_blocked".to_string());
            return (
                StatusCode::CONFLICT,
                ResponseJson(ErasureBlockedResponse { error, blockers }),
            )
                .into_response();
        }
        E::InvalidLookup => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Provide exactly one of email or user_id",
        ),
        E::InvalidUserId => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "user_id must be a valid UUID",
        ),
        E::ConfirmEmailMismatch => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "confirm_email_mismatch",
            "confirm_email does not match the user's current email",
        ),
        E::Internal(e) => {
            tracing::error!(
                user_id = user_id.map(tracing::field::display),
                error_kind = repository_error_kind(&e),
                "User erasure failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Internal server error",
            )
        }
    };
    (
        status,
        ResponseJson(ErrorResponse::new(
            message.to_string(),
            error_type.to_string(),
        )),
    )
        .into_response()
}

/// Erase a user (GDPR Art. 17). Irreversible. Run the preview first. Admin session only.
#[utoipa::path(
    post,
    path = "/v1/admin/users/{user_id}/erasure",
    tag = "Admin",
    params(("user_id" = String, Path, description = "User ID")),
    request_body = EraseUserRequest,
    responses(
        (status = 200, description = "User erased (or already erased)", body = EraseUserResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
        (status = 409, description = "Erasure blocked; body includes `blockers`", body = ErasureBlockedResponse),
        (status = 422, description = "confirm_email mismatch", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Admin API tokens cannot erase", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(("session_token" = []))
)]
pub async fn erase_user(
    State(app_state): State<AdminAppState>,
    Path(user_id): Path<uuid::Uuid>,
    Extension(admin_user): Extension<AdminUser>,
    ResponseJson(req): ResponseJson<EraseUserRequest>,
) -> Result<ResponseJson<EraseUserResponse>, axum::response::Response> {
    let r = app_state
        .user_erasure_service
        .erase(
            user_id,
            admin_user.0.id,
            &req.confirm_email,
            req.requested_at,
        )
        .await
        .map_err(|e| erasure_error_to_response(Some(user_id), e))?;

    Ok(ResponseJson(EraseUserResponse {
        user_id: r.user_id.to_string(),
        lifecycle: services::lifecycle::UserLifecycle::Erased,
        already_erased: r.already_erased,
        erased_organization_ids: r
            .erased_organization_ids
            .iter()
            .map(uuid::Uuid::to_string)
            .collect(),
    }))
}

/// Preview a user erasure: blockers and what would be erased (Admin only, read).
#[utoipa::path(
    post,
    path = "/v1/admin/users/{user_id}/erasure/preview",
    tag = "Admin",
    params(("user_id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "Erasure preview", body = ErasurePreviewResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(("session_token" = []))
)]
pub async fn preview_user_erasure(
    State(app_state): State<AdminAppState>,
    Path(user_id): Path<uuid::Uuid>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<ResponseJson<ErasurePreviewResponse>, axum::response::Response> {
    let plan = app_state
        .user_erasure_service
        .preview(user_id)
        .await
        .map_err(|e| erasure_error_to_response(Some(user_id), e))?;

    Ok(ResponseJson(ErasurePreviewResponse {
        user_id: plan.user_id.to_string(),
        lifecycle: plan.lifecycle,
        blockers: plan.blockers.clone(),
        erased_organizations: plan
            .erased_organizations
            .iter()
            .map(|o| ErasedOrganizationResponse {
                id: o.id.to_string(),
                lifecycle: o.lifecycle,
                workspaces: o.workspaces,
                api_keys: o.api_keys,
                conversations: o.conversations,
                files: o.files,
            })
            .collect(),
        retained_organizations: plan
            .retained_organizations
            .iter()
            .map(|o| RetainedOrganizationResponse {
                id: o.id.to_string(),
                role: o.role.clone(),
            })
            .collect(),
        log: plan.log.map(|l| ErasureLogResponse {
            requested_at: l.requested_at,
            erased_at: l.erased_at,
        }),
    }))
}

/// Find erasure records by the erased person's email or user id (Admin session only).
/// The email travels in the body, never the URL. No match is a 200 with an empty list.
#[utoipa::path(
    post,
    path = "/v1/admin/user-erasures/lookup",
    tag = "Admin",
    request_body = LookupUserErasuresRequest,
    responses(
        (status = 200, description = "Matching erasure records, newest first (at most 100)", body = LookupUserErasuresResponse),
        (status = 400, description = "Exactly one of a non-blank email or a valid user_id UUID is required", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Admin API tokens cannot look up erasures", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(("session_token" = []))
)]
pub async fn lookup_user_erasures(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    ResponseJson(req): ResponseJson<LookupUserErasuresRequest>,
) -> Result<ResponseJson<LookupUserErasuresResponse>, axum::response::Response> {
    let user_id = match req.user_id.as_deref() {
        None => None,
        Some(raw) => Some(raw.trim().parse::<uuid::Uuid>().map_err(|_| {
            erasure_error_to_response(
                None,
                services::user_erasure::UserErasureError::InvalidUserId,
            )
        })?),
    };
    let records = app_state
        .user_erasure_service
        .lookup(req.email.as_deref(), user_id)
        .await
        .map_err(|e| erasure_error_to_response(user_id, e))?;

    let orgs = |orgs: Vec<services::user_erasure::ErasureRecordOrg>| {
        orgs.into_iter()
            .map(|o| ErasureLookupOrganizationResponse {
                organization_id: o.organization_id.to_string(),
                lifecycle: o.lifecycle,
            })
            .collect::<Vec<_>>()
    };
    Ok(ResponseJson(LookupUserErasuresResponse {
        erasures: records
            .into_iter()
            .map(|r| UserErasureRecordResponse {
                user_id: r.user_id.to_string(),
                user_lifecycle: r.user_lifecycle,
                admin_user_id: r.admin_user_id.to_string(),
                requested_at: r.requested_at,
                erased_at: r.erased_at,
                erased_organizations: orgs(r.erased_organizations),
                retained_organizations: orgs(r.retained_organizations),
            })
            .collect(),
    }))
}

#[cfg(test)]
mod repository_error_kind_tests {
    use super::repository_error_kind;
    use services::common::RepositoryError as R;

    #[test]
    fn maps_repository_errors_to_static_kinds() {
        let cases: Vec<(anyhow::Error, &str)> = vec![
            (R::NotFound("x".into()).into(), "not_found"),
            (R::TransactionConflict.into(), "transaction_conflict"),
            (R::QueryTimeout.into(), "query_timeout"),
            (R::PoolError(anyhow::anyhow!("secret")).into(), "pool"),
            (
                R::DatabaseError(anyhow::anyhow!("secret")).into(),
                "database",
            ),
            (
                R::DataConversionError(anyhow::anyhow!("secret")).into(),
                "data_conversion",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(repository_error_kind(&error), expected);
        }
    }

    #[test]
    fn sees_through_context_and_defaults_to_other() {
        use anyhow::Context;
        let wrapped = Err::<(), _>(R::TransactionConflict)
            .context("while erasing")
            .unwrap_err();
        assert_eq!(repository_error_kind(&wrapped), "transaction_conflict");
        assert_eq!(repository_error_kind(&anyhow::anyhow!("boom")), "other");
    }
}
