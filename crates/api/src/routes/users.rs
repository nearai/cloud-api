use crate::{middleware::AuthenticatedUser, models::ErrorResponse, routes::api::AppState};
use axum::{
    extract::{Extension, Json, Path, State},
    http::StatusCode,
};
use database::{Session, User};
use serde::Deserialize;
use services::organization::ports::OrganizationRepository;
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
        ("bearer" = []),
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

/// Get a user by ID
///
/// Returns the profile of a user by their ID. Users can access their own profile
/// or profiles of users in the same organization.
#[utoipa::path(
    get,
    path = "/users/{user_id}",
    tag = "Users",
    params(
        ("user_id" = Uuid, Path, description = "User ID")
    ),
    responses(
        (status = 200, description = "User profile", body = crate::models::UserResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not in same organization", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn get_user(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<crate::models::UserResponse>, StatusCode> {
    debug!("Getting user: {} for user: {}", user_id, current_user.0.id);

    // Users can get their own profile or profiles of users in the same organization
    if current_user.0.id != user_id {
        // Check if they share an organization
        let query = "
            SELECT COUNT(*) as count
            FROM organization_members om1
            JOIN organization_members om2 ON om1.organization_id = om2.organization_id
            WHERE om1.user_id = $1 AND om2.user_id = $2
        ";

        let client = app_state.db.pool().get().await.map_err(|e| {
            error!("Failed to get database connection: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        let row = client
            .query_one(query, &[&current_user.0.id, &user_id])
            .await
            .map_err(|e| {
                error!("Failed to check shared organizations: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        let count: i64 = row.get("count");
        if count == 0 {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    match app_state.db.users.get_by_id(user_id).await {
        Ok(Some(user)) => Ok(Json(db_user_to_api_user(&user))),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get user: {}", e);
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
        ("bearer" = []),
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
        ("bearer" = []),
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

    get_user_organizations_by_id(
        State(app_state),
        Extension(current_user.clone()),
        Path(current_user.0.id),
    )
    .await
}

/// Get user's organizations by ID
///
/// Returns all organizations for a specific user. Users can only access their own organizations.
#[utoipa::path(
    get,
    path = "/users/{user_id}/organizations",
    tag = "Users",
    params(
        ("user_id" = Uuid, Path, description = "User ID")
    ),
    responses(
        (status = 200, description = "List of user's organizations", body = Vec<crate::models::OrganizationResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - can only access own organizations", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn get_user_organizations_by_id(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<Vec<crate::models::OrganizationResponse>>, StatusCode> {
    debug!(
        "Getting organizations for user: {} requested by: {}",
        user_id, current_user.0.id
    );

    // Users can only get their own orgs
    if current_user.0.id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }

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

    let rows = client.query(query, &[&user_id]).await.map_err(|e| {
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
/// Returns all active sessions for a specific user. Users can only access their own sessions.
#[utoipa::path(
    get,
    path = "/users/{user_id}/sessions",
    tag = "Users",
    params(
        ("user_id" = Uuid, Path, description = "User ID")
    ),
    responses(
        (status = 200, description = "List of user sessions", body = Vec<crate::models::SessionResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - can only access own sessions", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn get_user_sessions(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<Vec<crate::models::SessionResponse>>, StatusCode> {
    debug!(
        "Getting sessions for user: {} requested by: {}",
        user_id, current_user.0.id
    );

    // Users can only get their own sessions
    if current_user.0.id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }

    match app_state.db.sessions.list_by_user(user_id).await {
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
/// Revokes a specific session for a user. Users can only revoke their own sessions.
#[utoipa::path(
    delete,
    path = "/users/{user_id}/sessions/{session_id}",
    tag = "Users",
    params(
        ("user_id" = Uuid, Path, description = "User ID"),
        ("session_id" = Uuid, Path, description = "Session ID to revoke")
    ),
    responses(
        (status = 204, description = "Session revoked successfully"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - can only revoke own sessions", body = ErrorResponse),
        (status = 404, description = "Session not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn revoke_user_session(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path((user_id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, StatusCode> {
    debug!(
        "Revoking session: {} for user: {} requested by: {}",
        session_id, user_id, current_user.0.id
    );

    // Users can only revoke their own sessions
    if current_user.0.id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }

    // Verify the session belongs to the user
    match app_state.db.sessions.get_by_id(session_id).await {
        Ok(Some(session)) => {
            if session.user_id != user_id {
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
/// Revokes all active sessions for a user. Users can only revoke their own sessions.
#[utoipa::path(
    delete,
    path = "/users/{user_id}/sessions",
    tag = "Users",
    params(
        ("user_id" = Uuid, Path, description = "User ID")
    ),
    responses(
        (status = 200, description = "All sessions revoked successfully", body = serde_json::Value),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - can only revoke own sessions", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn revoke_all_user_sessions(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    debug!(
        "Revoking all sessions for user: {} requested by: {}",
        user_id, current_user.0.id
    );

    // Users can only revoke their own sessions
    if current_user.0.id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }

    match app_state.db.sessions.revoke_all_for_user(user_id).await {
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
