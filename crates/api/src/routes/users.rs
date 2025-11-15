use crate::{
    conversions::{
        authenticated_user_to_user_id, services_invitation_to_api, services_member_to_api_member,
        services_user_to_api_user,
    },
    middleware::AuthenticatedUser,
    models::ErrorResponse,
    routes::api::AppState,
};
use axum::{
    extract::{Extension, Json, Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use services::{organization::OrganizationError, user::UserServiceError};
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

    let user_id = services::auth::UserId(user.0.id);

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

    // Get user's organizations with roles
    let organizations = match app_state
        .organization_service
        .list_organizations_for_user(user_id.clone(), 100, 0, None, None)
        .await
    {
        Ok(orgs) => {
            let mut user_orgs = Vec::new();
            for org in orgs {
                // Get user's role in this organization
                if let Ok(Some(role)) = app_state
                    .organization_service
                    .get_user_role(org.id.clone(), user_id.clone())
                    .await
                {
                    user_orgs.push(crate::models::UserOrganizationResponse {
                        id: org.id.0.to_string(),
                        name: org.name,
                        description: org.description,
                        role: crate::conversions::services_role_to_api_role(role),
                        is_active: org.is_active,
                        created_at: org.created_at,
                    });
                }
            }
            user_orgs
        }
        Err(_) => {
            error!("Failed to list organizations for user");
            Vec::new()
        }
    };

    // Get user's workspaces from all their organizations
    let mut workspaces = Vec::new();
    for org_response in &organizations {
        let org_id = match Uuid::parse_str(&org_response.id) {
            Ok(id) => services::organization::OrganizationId(id),
            Err(_) => continue,
        };

        if let Ok(org_workspaces) = app_state
            .workspace_service
            .list_workspaces_for_organization(org_id.clone(), user_id.clone())
            .await
        {
            for workspace in org_workspaces {
                workspaces.push(crate::models::UserWorkspaceResponse {
                    id: workspace.id.0.to_string(),
                    name: workspace.name,
                    display_name: Some(workspace.display_name),
                    organization_id: workspace.organization_id.0.to_string(),
                    is_active: workspace.is_active,
                    created_at: workspace.created_at,
                });
            }
        }
    }

    // Build response with all data
    let response = crate::conversions::services_user_to_api_user_with_relations(
        &user_data,
        organizations,
        workspaces,
    );

    Ok(Json(response))
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

    let user_id = services::auth::UserId(user.0.id);

    match app_state
        .user_service
        .update_profile(user_id, request.display_name, request.avatar_url)
        .await
    {
        Ok(updated_user) => Ok(Json(services_user_to_api_user(&updated_user))),
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

    let user_id = services::auth::UserId(current_user.0.id);

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

    let user_id = services::auth::UserId(current_user.0.id);
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

    let user_id = services::auth::UserId(current_user.0.id);

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
        (status = 200, description = "Access token created successfully", body = crate::models::AccessTokenResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("refresh_token" = [])
    )
)]
pub async fn create_access_token(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<crate::models::AccessTokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Creating access token for user: {}", user.0.id);

    match app_state.auth_service.create_session_access_token(
        services::auth::UserId(user.0.id),
        app_state.config.auth.encoding_key.to_string(),
        1, // 1 hour expiration
    ) {
        Ok(access_token) => Ok(Json(crate::models::AccessTokenResponse { access_token })),
        Err(_) => {
            error!("Failed to create access token");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to create access token".to_string(),
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
        (status = 200, description = "List of pending invitations", body = Vec<crate::models::OrganizationInvitationResponse>),
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
    Json<Vec<crate::models::OrganizationInvitationResponse>>,
    (StatusCode, Json<ErrorResponse>),
> {
    debug!("Listing invitations for user: {}", user.0.email);

    match app_state
        .organization_service
        .list_user_invitations(&user.0.email)
        .await
    {
        Ok(invitations) => {
            let responses: Vec<crate::models::OrganizationInvitationResponse> = invitations
                .into_iter()
                .map(services_invitation_to_api)
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
    debug!(
        "User {} accepting invitation {}",
        user.0.email, invitation_id
    );

    let user_id = authenticated_user_to_user_id(user.clone());

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
    debug!(
        "User {} declining invitation {}",
        user.0.email, invitation_id
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
    debug!("User {} accepting invitation by token", user.0.email);

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
