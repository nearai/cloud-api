use axum::{
    extract::{Json, Path, Query, State, Extension},
    http::StatusCode,
};
use database::{User, Session};
use serde::Deserialize;
use uuid::Uuid;
use tracing::{debug, error};
use crate::{middleware::AuthenticatedUser, routes::api::AppState};

/// Query parameters for searching users
#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 { 20 }

/// Query parameters for listing
#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// User profile update request
#[derive(Debug, Deserialize)]
pub struct UpdateUserProfileRequest {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

/// Get current user
pub async fn get_current_user(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<User>, StatusCode> {
    debug!("Getting current user: {}", user.0.id);
    
    match app_state.db.users.get_by_id(user.0.id).await {
        Ok(Some(user)) => Ok(Json(user)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get current user: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Get a user by ID
pub async fn get_user(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<User>, StatusCode> {
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
        
        let row = client.query_one(query, &[&current_user.0.id, &user_id]).await
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
        Ok(Some(user)) => Ok(Json(user)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get user: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Update current user's profile
pub async fn update_current_user_profile(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<UpdateUserProfileRequest>,
) -> Result<Json<User>, StatusCode> {
    debug!("Updating profile for user: {}", user.0.id);
    
    match app_state.db.users.update_profile(
        user.0.id,
        request.display_name,
        request.avatar_url,
    ).await {
        Ok(updated_user) => Ok(Json(updated_user)),
        Err(e) => {
            error!("Failed to update user profile: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// List all users (shows users in same organizations)
pub async fn list_users(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<User>>, StatusCode> {
    debug!("Listing users for user: {}", user.0.id);
    
    // List users in the same organizations as the current user
    let query = "
        SELECT DISTINCT u.*
        FROM users u
        JOIN organization_members om ON u.id = om.user_id
        WHERE om.organization_id IN (
            SELECT organization_id 
            FROM organization_members 
            WHERE user_id = $1
        )
        ORDER BY u.created_at DESC
        LIMIT $2 OFFSET $3
    ";
    
    let client = app_state.db.pool().get().await.map_err(|e| {
        error!("Failed to get database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    
    let rows = client.query(query, &[&user.0.id, &params.limit, &params.offset]).await
        .map_err(|e| {
            error!("Failed to list users: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    let mut users = Vec::new();
    for row in rows {
        if let Ok(Some(user)) = app_state.db.users.get_by_id(row.get("id")).await {
            users.push(user);
        }
    }
    
    Ok(Json(users))
}

/// Search users
pub async fn search_users(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<User>>, StatusCode> {
    debug!("Searching users with query: {} for user: {}", params.q, user.0.id);
    
    // Search for users in the same organizations as the current user
    let query = "
        SELECT DISTINCT u.*
        FROM users u
        JOIN organization_members om ON u.id = om.user_id
        WHERE om.organization_id IN (
            SELECT organization_id 
            FROM organization_members 
            WHERE user_id = $1
        )
        AND (u.username ILIKE $2 OR u.email ILIKE $2)
        LIMIT $3
    ";
    
    let client = app_state.db.pool().get().await.map_err(|e| {
        error!("Failed to get database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    
    let search_pattern = format!("%{}%", params.q);
    let rows = client.query(query, &[&user.0.id, &search_pattern, &params.limit]).await
        .map_err(|e| {
            error!("Failed to search users: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    
    let mut users = Vec::new();
    for row in rows {
        if let Ok(Some(user)) = app_state.db.users.get_by_id(row.get("id")).await {
            users.push(user);
        }
    }
    
    Ok(Json(users))
}

/// Get user's organizations (current user)
pub async fn get_user_organizations(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<database::Organization>>, StatusCode> {
    debug!("Getting organizations for current user: {}", current_user.0.id);
    
    get_user_organizations_by_id(
        State(app_state),
        Extension(current_user.clone()),
        Path(current_user.0.id)
    ).await
}

/// Get user's organizations by ID
pub async fn get_user_organizations_by_id(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<Vec<database::Organization>>, StatusCode> {
    debug!("Getting organizations for user: {} requested by: {}", user_id, current_user.0.id);
    
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
            organizations.push(org);
        }
    }
    
    Ok(Json(organizations))
}

/// Get user's sessions
pub async fn get_user_sessions(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<Vec<Session>>, StatusCode> {
    debug!("Getting sessions for user: {} requested by: {}", user_id, current_user.0.id);
    
    // Users can only get their own sessions
    if current_user.0.id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }
    
    match app_state.db.sessions.list_by_user(user_id).await {
        Ok(sessions) => Ok(Json(sessions)),
        Err(e) => {
            error!("Failed to get user sessions: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Revoke a user session
pub async fn revoke_user_session(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path((user_id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, StatusCode> {
    debug!("Revoking session: {} for user: {} requested by: {}", session_id, user_id, current_user.0.id);
    
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
pub async fn revoke_all_user_sessions(
    State(app_state): State<AppState>,
    Extension(current_user): Extension<AuthenticatedUser>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    debug!("Revoking all sessions for user: {} requested by: {}", user_id, current_user.0.id);
    
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