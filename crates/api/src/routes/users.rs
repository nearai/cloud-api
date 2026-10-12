use crate::{
    conversions::{
        authenticated_user_to_user_id, services_invitation_to_api,
        services_invitation_to_api_with_org, services_member_to_api_member,
        services_user_to_api_user,
    },
    middleware::AuthenticatedUser,
    models::ErrorResponse,
    routes::api::AppState,
};
use axum::{
    extract::{Extension, Json, Path, State},
    http::{header::RETRY_AFTER, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde::Serialize;
use services::{
    aml::{AmlError, AmlFlow},
    auth::AuthError,
    auth::UserId,
    contact_email::ContactEmailError,
    organization::OrganizationError,
    user::UserServiceError,
};
use tracing::{debug, error};
use utoipa::ToSchema;
use uuid::Uuid;

/// Convert service Session (refresh token) to API RefreshTokenResponse
fn services_session_to_api_refresh_token(
    session: &services::auth::Session,
) -> crate::models::RefreshTokenResponse {
    crate::models::RefreshTokenResponse {
        id: session.id.0.to_string(), // SessionId is now Uuid
        user_id: session.user_id.0.to_string(),
        created_at: session.created_at,
        expires_at: session.expires_at,
        ip_address: session.ip_address.clone(),
        user_agent: session.user_agent.clone(),
    }
}

/// User profile update request
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateUserProfileRequest {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UserStatusResponse {
    pub status: String,
}

/// Request to add a contact email. No `Debug`: the body is personal data.
#[derive(Deserialize, ToSchema)]
pub struct RequestContactEmailRequest {
    pub email: String,
}

/// Request to confirm a contact email. No `Debug`: the token is a credential.
#[derive(Deserialize, ToSchema)]
pub struct VerifyContactEmailRequest {
    /// Token from the link in the verification email
    pub token: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ContactEmailStatusResponse {
    /// `verification_sent` after a request, `verified` after a confirmation
    pub status: String,
}

/// Map a contact email error to its HTTP response. A rate-limited request carries
/// `Retry-After` with the seconds until the next send is allowed.
fn contact_email_error_response(error: ContactEmailError) -> Response {
    let (status, message, error_type) = match &error {
        ContactEmailError::Disabled => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Contact email verification is not enabled",
            "contact_email_verification_disabled",
        ),
        ContactEmailError::UserNotFound => (StatusCode::NOT_FOUND, "User not found", "not_found"),
        ContactEmailError::NotSupported => (
            StatusCode::FORBIDDEN,
            "A contact email can only be added to accounts that sign in with a NEAR wallet",
            "forbidden",
        ),
        ContactEmailError::InvalidEmail(message) => {
            (StatusCode::BAD_REQUEST, *message, "bad_request")
        }
        ContactEmailError::RateLimited { .. } => (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many verification emails requested. Try again later.",
            "rate_limit_exceeded",
        ),
        ContactEmailError::InvalidToken => (
            StatusCode::BAD_REQUEST,
            "This verification link is invalid or has expired",
            "invalid_verification_token",
        ),
        ContactEmailError::AddressInUse => (
            StatusCode::CONFLICT,
            "This email address is already used by another account",
            "conflict",
        ),
        ContactEmailError::SendFailed => (
            StatusCode::BAD_GATEWAY,
            "The verification email could not be sent. Try again later.",
            "email_delivery_failed",
        ),
        ContactEmailError::Internal(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to process the contact email request",
            "internal_server_error",
        ),
    };
    let body = Json(ErrorResponse::new(
        message.to_string(),
        error_type.to_string(),
    ));
    match error {
        ContactEmailError::RateLimited { retry_after_secs } => (
            status,
            [(RETRY_AFTER, HeaderValue::from(retry_after_secs))],
            body,
        )
            .into_response(),
        _ => (status, body).into_response(),
    }
}

impl UpdateUserProfileRequest {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(display_name) = &self.display_name {
            crate::routes::common::validate_max_length(
                display_name,
                "user display_name",
                crate::consts::MAX_NAME_LENGTH,
            )?;
        }

        if let Some(avatar_url) = &self.avatar_url {
            // URLs can be long, but we still cap to a reasonable length
            crate::routes::common::validate_max_length(
                avatar_url,
                "avatar_url",
                crate::consts::MAX_AVATAR_URL_LENGTH,
            )?;
        }

        Ok(())
    }
}

/// Get current user
///
/// Returns the profile of the currently authenticated user, including their organizations and workspaces.
#[utoipa::path(
    get,
    path = "/v1/users/me",
    tag = "Users",
    responses(
        (status = 200, description = "Current user profile", body = crate::models::UserResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_current_user(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<crate::models::UserResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Getting current user: {}", user.0.id);

    let user_id = UserId(user.0.id);

    // Get user information
    let user_data = match app_state.user_service.get_user(user_id.clone()).await {
        Ok(user) => user,
        Err(UserServiceError::UserNotFound) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new(
                    "User not found".to_string(),
                    "not_found".to_string(),
                )),
            ))
        }
        Err(_) => {
            error!("Failed to get current user");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to get current user".to_string(),
                    "internal_server_error".to_string(),
                )),
            ));
        }
    };

    // Default-dependent clients use the first active membership. Sort in SQL before
    // pagination by joined_at ASC, organization ID ASC (not organization creation).
    // Get user's organizations with roles — single JOIN query (replaces prior N+1 pattern of
    // list_organizations_for_user + N × get_user_role).
    let organizations = match app_state
        .organization_service
        .list_organizations_with_roles_for_user(
            user_id.clone(),
            100,
            0,
            Some(services::organization::OrganizationOrderBy::JoinedAt),
            Some(services::organization::OrganizationOrderDirection::Asc),
        )
        .await
    {
        Ok(orgs) => orgs
            .into_iter()
            .map(|org_with_role| crate::models::UserOrganizationResponse {
                id: org_with_role.organization.id.0.to_string(),
                name: org_with_role.organization.name,
                description: org_with_role.organization.description,
                role: crate::conversions::services_role_to_api_role(org_with_role.role),
                is_active: org_with_role.organization.is_active,
                created_at: org_with_role.organization.created_at,
            })
            .collect(),
        Err(_) => {
            error!("Failed to list organizations for user");
            Vec::new()
        }
    };

    // Get user's workspaces across all their organizations — single JOIN query (replaces prior
    // N+1 pattern of iterating over orgs and calling list_workspaces_for_organization per org,
    // which also triggered an is_member check per org).
    let workspaces = match app_state
        .workspace_service
        .list_workspaces_for_user(user_id.clone(), 500)
        .await
    {
        Ok(ws) => ws
            .into_iter()
            .map(|workspace| crate::models::UserWorkspaceResponse {
                id: workspace.id.0.to_string(),
                name: workspace.name,
                organization_id: workspace.organization_id.0.to_string(),
                is_active: workspace.is_active,
                created_at: workspace.created_at,
            })
            .collect(),
        Err(_) => {
            error!("Failed to list workspaces for user");
            Vec::new()
        }
    };

    // Build response with all data
    let response = crate::conversions::services_user_to_api_user_with_relations(
        &user_data,
        app_state
            .contact_email_service
            .needs_contact_email(&user_data),
        organizations,
        workspaces,
    );

    Ok(Json(response))
}

/// Get current user's account eligibility status
///
/// Returns a minimal eligibility response for frontend preflight checks. AML/KYT
/// provider details are never exposed in this public response.
#[utoipa::path(
    get,
    path = "/v1/users/me/status",
    tag = "Users",
    responses(
        (status = 200, description = "Current account is eligible", body = UserStatusResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Current account is not eligible", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_user_status(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<UserStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    match app_state
        .aml_service
        .check_authenticated_near_user(
            user.0.id,
            &user.0.auth_provider,
            &user.0.provider_user_id,
            AmlFlow::UserStatus,
        )
        .await
    {
        Ok(_) => Ok(Json(UserStatusResponse {
            status: "ok".to_string(),
        })),
        Err(AmlError::AccountBlocked) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Account error".to_string(),
                "account_error".to_string(),
            )),
        )),
        Err(error) => {
            error!(error = %error, "Failed to get user account status");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to check account status".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Update current user's profile
///
/// Updates the profile information for the currently authenticated user.
#[utoipa::path(
    patch,
    path = "/v1/users/me",
    tag = "Users",
    request_body = UpdateUserProfileRequest,
    responses(
        (status = 200, description = "Updated user profile", body = crate::models::UserResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn update_current_user_profile(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<UpdateUserProfileRequest>,
) -> Result<Json<crate::models::UserResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Updating profile for user: {}", user.0.id);

    if let Err(msg) = request.validate() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(msg, "bad_request".to_string())),
        ));
    }

    let user_id = UserId(user.0.id);

    match app_state
        .user_service
        .update_profile(user_id, request.display_name, request.avatar_url)
        .await
    {
        Ok(updated_user) => Ok(Json(services_user_to_api_user(
            &updated_user,
            app_state
                .contact_email_service
                .needs_contact_email(&updated_user),
        ))),
        Err(UserServiceError::UserNotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "User not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(_) => {
            error!("Failed to update user profile");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to update current user".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Add a contact email
///
/// For accounts that sign in with a NEAR wallet and so have no email address.
/// Sends a single-use verification link, valid for 24 hours, to the given
/// address. The address is only saved once the link is confirmed with
/// `POST /v1/users/contact-email/verify`. The answer is the same whether or not
/// the address belongs to another account; that is reported at confirmation.
#[utoipa::path(
    post,
    path = "/v1/users/me/contact-email",
    tag = "Users",
    request_body = RequestContactEmailRequest,
    responses(
        (status = 200, description = "Verification email sent", body = ContactEmailStatusResponse),
        (status = 400, description = "Invalid email address", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "The account already signs in with an email address", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
        (status = 429, description = "Too many verification emails for this user or this address; see the Retry-After header", body = ErrorResponse),
        (status = 502, description = "The verification email could not be sent", body = ErrorResponse),
        (status = 503, description = "Contact email verification is not enabled", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn request_contact_email(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<RequestContactEmailRequest>,
) -> Result<Json<ContactEmailStatusResponse>, Response> {
    debug!("Contact email requested by user: {}", user.0.id);

    match app_state
        .contact_email_service
        .request_verification(UserId(user.0.id), &request.email)
        .await
    {
        Ok(()) => Ok(Json(ContactEmailStatusResponse {
            status: "verification_sent".to_string(),
        })),
        Err(e) => {
            if matches!(e, ContactEmailError::Internal(_)) {
                error!(user_id = %user.0.id, "Failed to request contact email verification");
            }
            Err(contact_email_error_response(e))
        }
    }
}

/// Confirm a contact email (public endpoint)
///
/// Confirms the address behind a verification token and records the opt-in.
/// No session is needed: the token from the emailed link is the credential, and
/// it works once. Fails with 409, changing nothing, when the address is another
/// account's login email or verified contact email.
#[utoipa::path(
    post,
    path = "/v1/users/contact-email/verify",
    tag = "Users",
    request_body = VerifyContactEmailRequest,
    responses(
        (status = 200, description = "Contact email verified", body = ContactEmailStatusResponse),
        (status = 400, description = "Unknown, expired or already used token", body = ErrorResponse),
        (status = 409, description = "The address is already used by another account", body = ErrorResponse),
        (status = 503, description = "Contact email verification is not enabled", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn verify_contact_email(
    State(app_state): State<AppState>,
    Json(request): Json<VerifyContactEmailRequest>,
) -> Result<Json<ContactEmailStatusResponse>, Response> {
    debug!("Confirming contact email by token");

    match app_state
        .contact_email_service
        .confirm(&request.token)
        .await
    {
        Ok(()) => Ok(Json(ContactEmailStatusResponse {
            status: "verified".to_string(),
        })),
        Err(e) => {
            if matches!(e, ContactEmailError::Internal(_)) {
                error!("Failed to confirm contact email");
            }
            Err(contact_email_error_response(e))
        }
    }
}

/// Get user's refresh tokens
///
/// Returns all active refresh tokens for the currently authenticated user.
#[utoipa::path(
    get,
    path = "/v1/users/me/refresh-tokens",
    tag = "Users",
    responses(
        (status = 200, description = "List of user refresh tokens", body = Vec<crate::models::RefreshTokenResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_user_refresh_tokens(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<crate::models::RefreshTokenResponse>>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Getting refresh tokens for user: {}", current_user.0.id);

    let user_id = UserId(current_user.0.id);

    match app_state.user_service.get_user_sessions(user_id).await {
        Ok(sessions) => {
            let api_refresh_tokens = sessions
                .iter()
                .map(services_session_to_api_refresh_token)
                .collect();
            Ok(Json(api_refresh_tokens))
        }
        Err(_) => {
            error!("Failed to get user refresh tokens");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to get user refresh tokens".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Revoke a user refresh token
///
/// Revokes a specific refresh token for the currently authenticated user.
#[utoipa::path(
    delete,
    path = "/v1/users/me/refresh-tokens/{refresh_token_id}",
    tag = "Users",
    params(
        ("refresh_token_id" = Uuid, Path, description = "Refresh token ID to revoke")
    ),
    responses(
        (status = 204, description = "Refresh token revoked successfully"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Refresh token not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn revoke_user_refresh_token(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path(refresh_token_id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Revoking refresh token: {} for user: {}",
        refresh_token_id, current_user.0.id
    );

    let user_id = UserId(current_user.0.id);
    let session_id = services::auth::SessionId(refresh_token_id);

    match app_state
        .user_service
        .revoke_session(user_id, session_id)
        .await
    {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) | Err(UserServiceError::SessionNotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Refresh token not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        // Don't leak that the refresh token exists
        Err(UserServiceError::Unauthorized(_)) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Refresh token not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(_) => {
            error!("Failed to revoke refresh token");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to revoke refresh tokens".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Revoke all tokens for a user
///
/// Revokes all tokens (both refresh tokens and access tokens) for the currently authenticated user.
/// This deletes all refresh tokens from the database and invalidates all JWT access tokens
/// by updating the user's tokens_revoked_at timestamp.
#[utoipa::path(
    delete,
    path = "/v1/users/me/tokens",
    tag = "Users",
    responses(
        (status = 200, description = "All tokens revoked successfully", body = serde_json::Value),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn revoke_all_user_tokens(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Revoking all tokens for user: {}", current_user.0.id);

    let user_id = UserId(current_user.0.id);

    match app_state.user_service.revoke_all_sessions(user_id).await {
        Ok(count) => Ok(Json(serde_json::json!({
            "message": format!("Revoked {} refresh tokens and invalidated all access tokens", count),
            "count": count
        }))),
        Err(_) => {
            error!("Failed to revoke all user tokens");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to revoke all user tokens".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Create a new access token
///
/// Creates a new short-lived access token using the current refresh token.
#[utoipa::path(
    post,
    path = "/v1/users/me/access-tokens",
    tag = "Users",
    responses(
        (status = 200, description = "Access token created successfully", body = crate::models::AccessAndRefreshTokenResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("refresh_token" = [])
    )
)]
pub async fn create_access_token(
    State(app_state): State<AppState>,
    headers: HeaderMap,
    Extension((session, user)): Extension<(services::auth::Session, AuthenticatedUser)>,
) -> Result<Json<crate::models::AccessAndRefreshTokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Creating access token & rotating refresh token for user: {}",
        user.0.id
    );

    // Rotate the refresh token session
    let old_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new(
                    "Refresh token is required".to_string(),
                    "unauthorized".to_string(),
                )),
            )
        })?;
    let expires_in_hours = 7 * 24;
    let result = app_state
        .auth_service
        .rotate_session(
            session.user_id,
            session.id,
            old_token,
            app_state.config.auth.encoding_key.to_string(),
            1,                // access token expires in 1 hour
            expires_in_hours, // refresh token expires in 7 days
        )
        .await;

    match result {
        Ok((access_token, refresh_session, refresh_token)) => {
            Ok(Json(crate::models::AccessAndRefreshTokenResponse {
                access_token,
                refresh_token,
                refresh_token_expiration: refresh_session.expires_at,
            }))
        }
        Err(AuthError::Unauthorized) => {
            tracing::warn!("Token rotation failed: invalid or already rotated token");
            Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new(
                    "Invalid or expired refresh token".to_string(),
                    "unauthorized".to_string(),
                )),
            ))
        }
        Err(e) => {
            error!("Failed to create access token and rotate refresh token: {e}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to create access token and rotate refresh token".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// List pending invitations for the current user
///
/// Returns all pending organization invitations for the authenticated user's email.
#[utoipa::path(
    get,
    path = "/v1/users/me/invitations",
    tag = "Users",
    responses(
        (status = 200, description = "List of pending invitations", body = Vec<crate::models::OrganizationInvitationWithOrgResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_user_invitations(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<
    Json<Vec<crate::models::OrganizationInvitationWithOrgResponse>>,
    (StatusCode, Json<ErrorResponse>),
> {
    debug!("Listing invitations for user_id={}", user.0.id);

    match app_state
        .organization_service
        .list_user_invitations(&user.0.email)
        .await
    {
        Ok(invitations) => {
            let responses = invitations
                .into_iter()
                .map(services_invitation_to_api_with_org)
                .collect();
            Ok(Json(responses))
        }
        Err(_) => {
            error!("Failed to list user invitations");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to list user invitations".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Accept an organization invitation
///
/// Accepts a pending invitation and adds the user as a member of the organization.
#[utoipa::path(
    post,
    path = "/v1/users/me/invitations/{invitation_id}/accept",
    tag = "Users",
    params(
        ("invitation_id" = Uuid, Path, description = "Invitation ID")
    ),
    responses(
        (status = 200, description = "Invitation accepted successfully", body = crate::models::AcceptInvitationResponse),
        (status = 400, description = "Bad request - invitation expired or invalid", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - invitation belongs to another user", body = ErrorResponse),
        (status = 404, description = "Invitation not found", body = ErrorResponse),
        (status = 409, description = "User is already a member", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn accept_invitation(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(invitation_id): Path<Uuid>,
) -> Result<Json<crate::models::AcceptInvitationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = authenticated_user_to_user_id(user.clone());
    debug!(
        user_id = %user_id.0,
        invitation_id = %invitation_id,
        action = "accept",
        "User invitation action requested"
    );

    match app_state
        .organization_service
        .accept_invitation(invitation_id, user_id, &user.0.email)
        .await
    {
        Ok(member) => {
            let response = crate::models::AcceptInvitationResponse {
                organization_member: services_member_to_api_member(member),
                message: "Successfully joined organization".to_string(),
            };
            Ok(Json(response))
        }
        Err(OrganizationError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Organization not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(OrganizationError::Unauthorized(msg)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(msg, "forbidden".to_string())),
        )),
        Err(OrganizationError::InvalidParams(msg)) => Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(msg, "bad_request".to_string())),
        )),
        Err(OrganizationError::AlreadyMember) => Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                "Already a member".to_string(),
                "conflict".to_string(),
            )),
        )),
        Err(_) => {
            error!("Failed to accept invitation");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to accept invitation".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Decline an organization invitation
///
/// Declines a pending invitation to join an organization.
#[utoipa::path(
    post,
    path = "/v1/users/me/invitations/{invitation_id}/decline",
    tag = "Users",
    params(
        ("invitation_id" = Uuid, Path, description = "Invitation ID")
    ),
    responses(
        (status = 200, description = "Invitation declined successfully"),
        (status = 400, description = "Bad request - invitation not pending", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - invitation belongs to another user", body = ErrorResponse),
        (status = 404, description = "Invitation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn decline_invitation(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(invitation_id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user_id = authenticated_user_to_user_id(user.clone());
    debug!(
        user_id = %user_id.0,
        invitation_id = %invitation_id,
        action = "decline",
        "User invitation action requested"
    );

    match app_state
        .organization_service
        .decline_invitation(invitation_id, &user.0.email)
        .await
    {
        Ok(()) => Ok(StatusCode::OK),
        Err(OrganizationError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Organization not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(OrganizationError::Unauthorized(msg)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(msg, "forbidden".to_string())),
        )),
        Err(OrganizationError::InvalidParams(msg)) => Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(msg, "bad_request".to_string())),
        )),
        Err(_) => {
            error!("Failed to decline invitation");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to decline invitation".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Get invitation details by token (public endpoint)
///
/// Returns invitation details for a specific token. This is a public endpoint
/// that allows users to view invitation details before logging in.
#[utoipa::path(
    get,
    path = "/v1/invitations/{token}",
    tag = "Invitations",
    params(
        ("token" = String, Path, description = "Invitation token")
    ),
    responses(
        (status = 200, description = "Invitation details", body = crate::models::OrganizationInvitationResponse),
        (status = 404, description = "Invitation not found", body = ErrorResponse),
        (status = 410, description = "Invitation expired or no longer pending", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn get_invitation_by_token(
    State(app_state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Json<crate::models::OrganizationInvitationResponse>, (StatusCode, Json<ErrorResponse>)>
{
    debug!("Getting invitation by token");

    match app_state
        .organization_service
        .get_invitation_by_token(&token)
        .await
    {
        Ok(invitation) => {
            let response = services_invitation_to_api(invitation);
            Ok(Json(response))
        }
        Err(OrganizationError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Organization not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(OrganizationError::InvalidParams(msg)) => Err((
            StatusCode::GONE,
            Json(ErrorResponse::new(msg, "gone".to_string())),
        )),
        Err(_) => {
            error!("Failed to get invitation by token");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to get invitation".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Accept invitation by token (requires authentication)
///
/// Accepts an invitation using its token. The authenticated user's email
/// must match the invitation email.
#[utoipa::path(
    post,
    path = "/v1/invitations/{token}/accept",
    tag = "Invitations",
    params(
        ("token" = String, Path, description = "Invitation token")
    ),
    responses(
        (status = 200, description = "Invitation accepted successfully", body = crate::models::AcceptInvitationResponse),
        (status = 400, description = "Bad request - invitation expired or invalid", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - invitation belongs to another user", body = ErrorResponse),
        (status = 404, description = "Invitation not found", body = ErrorResponse),
        (status = 409, description = "User is already a member", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn accept_invitation_by_token(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(token): Path<String>,
) -> Result<Json<crate::models::AcceptInvitationResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("User_id={} accepting invitation by token", user.0.id);

    let user_id = authenticated_user_to_user_id(user.clone());

    match app_state
        .organization_service
        .accept_invitation_by_token(&token, user_id, &user.0.email)
        .await
    {
        Ok(member) => {
            let response = crate::models::AcceptInvitationResponse {
                organization_member: services_member_to_api_member(member),
                message: "Successfully joined organization".to_string(),
            };
            Ok(Json(response))
        }
        Err(OrganizationError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Organization not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(OrganizationError::Unauthorized(msg)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(msg, "forbidden".to_string())),
        )),
        Err(OrganizationError::InvalidParams(msg)) => Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(msg, "bad_request".to_string())),
        )),
        Err(OrganizationError::AlreadyMember) => Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                "Already a member".to_string(),
                "conflict".to_string(),
            )),
        )),
        Err(_) => {
            error!("Failed to accept invitation by token");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to accept invitation".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}
