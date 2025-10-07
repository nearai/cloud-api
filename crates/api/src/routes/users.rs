use crate::{
    conversions::{
        authenticated_user_to_user_id, services_invitation_to_api, services_member_to_api_member,
    },
    middleware::AuthenticatedUser,
    models::ErrorResponse,
    routes::api::AppState,
};
use axum::{
    extract::{Extension, Json, Path, State},
    http::StatusCode,
};
use database::{Session, User};
use serde::Deserialize;
use services::organization::{ports::OrganizationRepository, OrganizationError};
use tracing::{debug, error};
use utoipa::ToSchema;
use uuid::Uuid;

/// Convert database User to API UserResponse
fn db_user_to_api_user(user: &User) -> crate::models::UserResponse {
    crate::models::UserResponse {
        id: user.id.to_string(),
        email: user.email.clone(),
        username: user.username.clone(),
        display_name: user.display_name.clone(),
        avatar_url: user.avatar_url.clone(),
        created_at: user.created_at,
        updated_at: user.updated_at,
        last_login_at: user.last_login_at,
        is_active: user.is_active,
        auth_provider: user.auth_provider.clone(),
    }
}

/// Convert database Session to API SessionResponse
fn db_session_to_api_session(session: &Session) -> crate::models::SessionResponse {
    crate::models::SessionResponse {
        id: session.id.to_string(),
        user_id: session.user_id.to_string(),
        created_at: session.created_at,
        expires_at: session.expires_at,
        ip_address: session.ip_address.clone(),
        user_agent: session.user_agent.clone(),
    }
}

/// Query parameters for searching users
#[derive(Debug, Deserialize, ToSchema)]
pub struct SearchParams {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    20
}

/// User profile update request
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateUserProfileRequest {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

/// Get current user
///
/// Returns the profile of the currently authenticated user.
#[utoipa::path(
    get,
    path = "/users/me",
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
) -> Result<Json<crate::models::UserResponse>, StatusCode> {
    debug!("Getting current user: {}", user.0.id);

    match app_state.db.users.get_by_id(user.0.id).await {
        Ok(Some(user)) => Ok(Json(db_user_to_api_user(&user))),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get current user: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Update current user's profile
///
/// Updates the profile information for the currently authenticated user.
#[utoipa::path(
    patch,
    path = "/users/me",
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
) -> Result<Json<crate::models::UserResponse>, StatusCode> {
    debug!("Updating profile for user: {}", user.0.id);

    match app_state
        .db
        .users
        .update_profile(user.0.id, request.display_name, request.avatar_url)
        .await
    {
        Ok(updated_user) => Ok(Json(db_user_to_api_user(&updated_user))),
        Err(e) => {
            error!("Failed to update user profile: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Get user's organizations (current user)
///
/// Returns all organizations that the currently authenticated user is a member of.
#[utoipa::path(
    get,
    path = "/users/me/organizations",
    tag = "Users",
    responses(
        (status = 200, description = "List of user's organizations", body = Vec<crate::models::OrganizationResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_user_organizations(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<crate::models::OrganizationResponse>>, StatusCode> {
    debug!(
        "Getting organizations for current user: {}",
        current_user.0.id
    );

    // Get all organization memberships for the user
    let query = "
        SELECT DISTINCT o.* 
        FROM organizations o
        JOIN organization_members om ON o.id = om.organization_id
        WHERE om.user_id = $1 AND o.is_active = true
        ORDER BY o.created_at DESC
    ";

    let client = app_state.db.pool().get().await.map_err(|e| {
        error!("Failed to get database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let rows = client
        .query(query, &[&current_user.0.id])
        .await
        .map_err(|e| {
            error!("Failed to query user organizations: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut organizations = Vec::new();
    for row in rows {
        if let Ok(Some(org)) = app_state.db.organizations.get_by_id(row.get("id")).await {
            let db_org = crate::conversions::services_org_to_api_org(org);
            organizations.push(db_org);
        }
    }

    Ok(Json(organizations))
}

/// Get user's sessions
///
/// Returns all active sessions for the currently authenticated user.
#[utoipa::path(
    get,
    path = "/users/me/sessions",
    tag = "Users",
    responses(
        (status = 200, description = "List of user sessions", body = Vec<crate::models::SessionResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_user_sessions(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<crate::models::SessionResponse>>, StatusCode> {
    debug!("Getting sessions for user: {}", current_user.0.id);

    match app_state.db.sessions.list_by_user(current_user.0.id).await {
        Ok(sessions) => {
            let api_sessions = sessions.iter().map(db_session_to_api_session).collect();
            Ok(Json(api_sessions))
        }
        Err(e) => {
            error!("Failed to get user sessions: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Revoke a user session
///
/// Revokes a specific session for the currently authenticated user.
#[utoipa::path(
    delete,
    path = "/users/me/sessions/{session_id}",
    tag = "Users",
    params(
        ("session_id" = Uuid, Path, description = "Session ID to revoke")
    ),
    responses(
        (status = 204, description = "Session revoked successfully"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Session not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn revoke_user_session(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path(session_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    debug!(
        "Revoking session: {} for user: {}",
        session_id, current_user.0.id
    );

    // Verify the session belongs to the user
    match app_state.db.sessions.get_by_id(session_id).await {
        Ok(Some(session)) => {
            if session.user_id != current_user.0.id {
                return Err(StatusCode::NOT_FOUND);
            }
        }
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get session: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    match app_state.db.sessions.revoke(session_id).await {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to revoke session: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Revoke all sessions for a user
///
/// Revokes all active sessions for the currently authenticated user.
#[utoipa::path(
    delete,
    path = "/users/me/sessions",
    tag = "Users",
    responses(
        (status = 200, description = "All sessions revoked successfully", body = serde_json::Value),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn revoke_all_user_sessions(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    debug!("Revoking all sessions for user: {}", current_user.0.id);

    match app_state
        .db
        .sessions
        .revoke_all_for_user(current_user.0.id)
        .await
    {
        Ok(count) => Ok(Json(serde_json::json!({
            "message": format!("Revoked {} sessions", count),
            "count": count
        }))),
        Err(e) => {
            error!("Failed to revoke all user sessions: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// List pending invitations for the current user
///
/// Returns all pending organization invitations for the authenticated user's email.
#[utoipa::path(
    get,
    path = "/users/me/invitations",
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
) -> Result<Json<Vec<crate::models::OrganizationInvitationResponse>>, StatusCode> {
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
        Err(e) => {
            error!("Failed to list user invitations: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Accept an organization invitation
///
/// Accepts a pending invitation and adds the user as a member of the organization.
#[utoipa::path(
    post,
    path = "/users/me/invitations/{invitation_id}/accept",
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
) -> Result<Json<crate::models::AcceptInvitationResponse>, StatusCode> {
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
        Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
        Err(OrganizationError::Unauthorized(_)) => Err(StatusCode::FORBIDDEN),
        Err(OrganizationError::InvalidParams(_)) => Err(StatusCode::BAD_REQUEST),
        Err(OrganizationError::AlreadyMember) => Err(StatusCode::CONFLICT),
        Err(e) => {
            error!("Failed to accept invitation: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Decline an organization invitation
///
/// Declines a pending invitation to join an organization.
#[utoipa::path(
    post,
    path = "/users/me/invitations/{invitation_id}/decline",
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
) -> Result<StatusCode, StatusCode> {
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
        Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
        Err(OrganizationError::Unauthorized(_)) => Err(StatusCode::FORBIDDEN),
        Err(OrganizationError::InvalidParams(_)) => Err(StatusCode::BAD_REQUEST),
        Err(e) => {
            error!("Failed to decline invitation: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Get invitation details by token (public endpoint)
///
/// Returns invitation details for a specific token. This is a public endpoint
/// that allows users to view invitation details before logging in.
#[utoipa::path(
    get,
    path = "/invitations/{token}",
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
) -> Result<Json<crate::models::OrganizationInvitationResponse>, StatusCode> {
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
        Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
        Err(OrganizationError::InvalidParams(_)) => {
            // Invitation expired or not pending
            Err(StatusCode::GONE) // 410 Gone
        }
        Err(e) => {
            error!("Failed to get invitation by token: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Accept invitation by token (requires authentication)
///
/// Accepts an invitation using its token. The authenticated user's email
/// must match the invitation email.
#[utoipa::path(
    post,
    path = "/invitations/{token}/accept",
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
) -> Result<Json<crate::models::AcceptInvitationResponse>, StatusCode> {
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
        Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
        Err(OrganizationError::Unauthorized(_)) => Err(StatusCode::FORBIDDEN),
        Err(OrganizationError::InvalidParams(_)) => Err(StatusCode::BAD_REQUEST),
        Err(OrganizationError::AlreadyMember) => Err(StatusCode::CONFLICT),
        Err(e) => {
            error!("Failed to accept invitation by token: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
