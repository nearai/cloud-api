use crate::{middleware::AuthenticatedUser, models::ErrorResponse, routes::api::AppState};
use axum::{
    extract::{Extension, Json, Path, State},
    http::{HeaderMap, StatusCode},
};
use services::{auth::AuthError, auth::UserId, user::UserServiceError};
use tracing::{debug, error};
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
    let mut response = crate::conversions::services_user_to_api_user_with_relations(
        &user_data,
        organizations,
        workspaces,
    );

    populate_staking_organization(&app_state, &user_data, &mut response).await;
    Ok(Json(response))
}

async fn populate_staking_organization(
    app: &AppState,
    user: &services::auth::User,
    response: &mut crate::models::UserResponse,
) {
    if user.auth_provider == "near" {
        if let Some(binding) = app.staking_farm_service.binding() {
            response.staking_organization_id = match binding
                .wallet_organization(&user.provider_user_id, user.id.0)
                .await
            {
                Ok(id) => id.map(|id| id.to_string()),
                Err(_) => {
                    // Repository errors can include customer identifiers; log the event, not raw SQL.
                    error!(user_id = %user.id.0, error_category = "staking_binding_lookup_failed", "Failed to resolve staking wallet binding");
                    None
                }
            };
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
