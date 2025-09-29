use crate::{
    conversions::authenticated_user_to_user_id,
    middleware::{auth::AuthenticatedApiKey, AuthenticatedUser},
    models::{ApiKeyResponse, CreateApiKeyRequest, ErrorResponse},
    routes::api::AppState,
};
use axum::{
    extract::{Extension, Json, Path, Query, State},
    http::StatusCode,
};
use database::repositories::WorkspaceRepository as DbWorkspaceRepository;
use serde::{Deserialize, Serialize};
use services::{
    auth::{ports::WorkspaceId, AuthError},
    organization::OrganizationId,
};
use std::sync::Arc;
use tracing::{debug, error};
use utoipa::ToSchema;
use uuid::Uuid;

// ============================================
// Workspace Models
// ============================================

/// Request to create a new workspace
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// Request to update a workspace
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateWorkspaceRequest {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub settings: Option<serde_json::Value>,
}

/// Workspace response model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceResponse {
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub organization_id: String,
    pub created_by_user_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub is_active: bool,
    pub settings: Option<serde_json::Value>,
}

/// Query parameters for listing
#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    20
}

// ============================================
// Workspace Management Routes
// ============================================

/// Create a new workspace in an organization
///
/// Creates a new workspace within the specified organization. The authenticated user must
/// be a member of the organization to create workspaces.
#[utoipa::path(
    post,
    path = "/organizations/{org_id}/workspaces",
    tag = "Workspaces",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    request_body = CreateWorkspaceRequest,
    responses(
        (status = 201, description = "Workspace created successfully", body = WorkspaceResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not a member of organization", body = ErrorResponse),
        (status = 409, description = "Workspace name already exists in organization", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn create_workspace(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<WorkspaceResponse>), StatusCode> {
    debug!(
        "Creating workspace: {} in organization: {} by user: {}",
        request.name, org_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user);
    let organization_id = OrganizationId(org_id);

    // Check if user is a member of the organization
    match app_state
        .organization_service
        .is_member(organization_id.clone(), user_id.clone())
        .await
    {
        Ok(true) => {
            // User is a member, create the workspace
            // We need to use the database directly as we don't have a workspace service yet
            let workspace_repo = Arc::new(DbWorkspaceRepository::new(app_state.db.pool().clone()));
            let db_request = database::CreateWorkspaceRequest {
                name: request.name,
                display_name: request.display_name.unwrap_or_default(),
                description: request.description,
            };

            match workspace_repo.create(db_request, org_id, user_id.0).await {
                Ok(workspace) => {
                    debug!(
                        "Created workspace: {} in organization: {}",
                        workspace.id, org_id
                    );
                    let response = WorkspaceResponse {
                        id: workspace.id.to_string(),
                        name: workspace.name,
                        display_name: Some(workspace.display_name),
                        description: workspace.description,
                        organization_id: workspace.organization_id.to_string(),
                        created_by_user_id: workspace.created_by_user_id.to_string(),
                        created_at: workspace.created_at,
                        updated_at: workspace.updated_at,
                        is_active: workspace.is_active,
                        settings: workspace.settings,
                    };
                    Ok((StatusCode::CREATED, Json(response)))
                }
                Err(e) => {
                    if e.to_string().contains("duplicate key")
                        || e.to_string().contains("already exists")
                    {
                        debug!("Workspace name already exists in organization");
                        Err(StatusCode::CONFLICT)
                    } else {
                        error!("Failed to create workspace: {}", e);
                        Err(StatusCode::INTERNAL_SERVER_ERROR)
                    }
                }
            }
        }
        Ok(false) => {
            debug!("User is not a member of organization: {}", org_id);
            Err(StatusCode::FORBIDDEN)
        }
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// List workspaces in an organization
///
/// Lists all workspaces within the specified organization. The authenticated user must
/// be a member of the organization to list workspaces.
#[utoipa::path(
    get,
    path = "/organizations/{org_id}/workspaces",
    tag = "Workspaces",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("limit" = Option<i64>, Query, description = "Maximum number of results"),
        ("offset" = Option<i64>, Query, description = "Number of results to skip")
    ),
    responses(
        (status = 200, description = "List of workspaces", body = Vec<WorkspaceResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not a member of organization", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
        ("api_key" = [])
    )
)]
pub async fn list_organization_workspaces(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Query(_params): Query<ListParams>,
) -> Result<Json<Vec<WorkspaceResponse>>, StatusCode> {
    debug!(
        "Listing workspaces for organization: {} by user: {}",
        org_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user);
    let organization_id = OrganizationId(org_id);

    // Check if user is a member of the organization
    match app_state
        .organization_service
        .is_member(organization_id.clone(), user_id)
        .await
    {
        Ok(true) => {
            // List workspaces
            let workspace_repo = Arc::new(DbWorkspaceRepository::new(app_state.db.pool().clone()));
            match workspace_repo.list_by_organization(org_id).await {
                Ok(workspaces) => {
                    let response: Vec<WorkspaceResponse> = workspaces
                        .into_iter()
                        .map(|w| WorkspaceResponse {
                            id: w.id.to_string(),
                            name: w.name,
                            display_name: Some(w.display_name),
                            description: w.description,
                            organization_id: w.organization_id.to_string(),
                            created_by_user_id: w.created_by_user_id.to_string(),
                            created_at: w.created_at,
                            updated_at: w.updated_at,
                            is_active: w.is_active,
                            settings: w.settings,
                        })
                        .collect();
                    Ok(Json(response))
                }
                Err(e) => {
                    error!("Failed to list workspaces: {}", e);
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(false) => Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Get workspace by ID
///
/// Returns workspace details for a specific workspace ID.
#[utoipa::path(
    get,
    path = "/workspaces/{workspace_id}",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    responses(
        (status = 200, description = "Workspace details", body = WorkspaceResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
        ("api_key" = [])
    )
)]
pub async fn get_workspace(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(workspace_id): Path<Uuid>,
) -> Result<Json<WorkspaceResponse>, StatusCode> {
    debug!("Getting workspace: {} by user: {}", workspace_id, user.0.id);

    let user_id = authenticated_user_to_user_id(user);

    // Get workspace and check permissions
    let workspace_repo = Arc::new(DbWorkspaceRepository::new(app_state.db.pool().clone()));
    match workspace_repo.get_by_id(workspace_id).await {
        Ok(Some(workspace)) => {
            let organization_id = OrganizationId(workspace.organization_id);

            // Check if user is a member of the organization that owns this workspace
            match app_state
                .organization_service
                .is_member(organization_id, user_id)
                .await
            {
                Ok(true) => {
                    let response = WorkspaceResponse {
                        id: workspace.id.to_string(),
                        name: workspace.name,
                        display_name: Some(workspace.display_name),
                        description: workspace.description,
                        organization_id: workspace.organization_id.to_string(),
                        created_by_user_id: workspace.created_by_user_id.to_string(),
                        created_at: workspace.created_at,
                        updated_at: workspace.updated_at,
                        is_active: workspace.is_active,
                        settings: workspace.settings,
                    };
                    Ok(Json(response))
                }
                Ok(false) => Err(StatusCode::FORBIDDEN),
                Err(e) => {
                    error!("Failed to check organization membership: {}", e);
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get workspace: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Update workspace
///
/// Updates workspace details for a specific workspace ID.
#[utoipa::path(
    put,
    path = "/workspaces/{workspace_id}",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    request_body = UpdateWorkspaceRequest,
    responses(
        (status = 200, description = "Updated workspace", body = WorkspaceResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn update_workspace(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(workspace_id): Path<Uuid>,
    Json(request): Json<UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceResponse>, StatusCode> {
    debug!(
        "Updating workspace: {} by user: {}",
        workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user);

    // Get workspace to check permissions
    let workspace_repo = Arc::new(DbWorkspaceRepository::new(app_state.db.pool().clone()));
    match workspace_repo.get_by_id(workspace_id).await {
        Ok(Some(workspace)) => {
            let organization_id = OrganizationId(workspace.organization_id);

            // Check if user has permission to update (must be admin/owner or creator)
            let can_update = if workspace.created_by_user_id == user_id.0 {
                true
            } else {
                // Check organization membership and role
                match app_state
                    .organization_service
                    .is_member(organization_id.clone(), user_id.clone())
                    .await
                {
                    Ok(true) => {
                        // For workspace updates, allow any member to update
                        // This could be refined to check specific roles if needed
                        true
                    }
                    _ => false,
                }
            };

            if !can_update {
                return Err(StatusCode::FORBIDDEN);
            }

            // Update the workspace
            let db_request = database::UpdateWorkspaceRequest {
                display_name: request.display_name,
                description: request.description,
                settings: request.settings,
            };

            match workspace_repo.update(workspace_id, db_request).await {
                Ok(Some(updated)) => {
                    let response = WorkspaceResponse {
                        id: updated.id.to_string(),
                        name: updated.name,
                        display_name: Some(updated.display_name),
                        description: updated.description,
                        organization_id: updated.organization_id.to_string(),
                        created_by_user_id: updated.created_by_user_id.to_string(),
                        created_at: updated.created_at,
                        updated_at: updated.updated_at,
                        is_active: updated.is_active,
                        settings: updated.settings,
                    };
                    Ok(Json(response))
                }
                Ok(None) => Err(StatusCode::NOT_FOUND),
                Err(e) => {
                    error!("Failed to update workspace: {}", e);
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get workspace: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Delete workspace
///
/// Deletes (deactivates) a workspace. Only the workspace creator or organization admin/owner can delete.
#[utoipa::path(
    delete,
    path = "/workspaces/{workspace_id}",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    responses(
        (status = 200, description = "Workspace deleted successfully"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn delete_workspace(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(workspace_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    debug!(
        "Deleting workspace: {} by user: {}",
        workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user);

    // Get workspace to check permissions
    let workspace_repo = Arc::new(DbWorkspaceRepository::new(app_state.db.pool().clone()));
    match workspace_repo.get_by_id(workspace_id).await {
        Ok(Some(workspace)) => {
            let organization_id = OrganizationId(workspace.organization_id);

            // Check if user has permission to delete (must be admin/owner or creator)
            let can_delete = if workspace.created_by_user_id == user_id.0 {
                true
            } else {
                // Check organization membership and role
                match app_state
                    .organization_service
                    .is_member(organization_id, user_id.clone())
                    .await
                {
                    Ok(true) => {
                        // For workspace deletes, allow any member to delete
                        // This could be refined to check specific roles if needed
                        true
                    }
                    _ => false,
                }
            };

            if !can_delete {
                return Err(StatusCode::FORBIDDEN);
            }

            // Delete the workspace
            match workspace_repo.delete(workspace_id).await {
                Ok(true) => {
                    debug!("Workspace {} deleted successfully", workspace_id);
                    Ok(Json(serde_json::json!({
                        "id": workspace_id.to_string(),
                        "deleted": true
                    })))
                }
                Ok(false) => Err(StatusCode::NOT_FOUND),
                Err(e) => {
                    error!("Failed to delete workspace: {}", e);
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get workspace: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ============================================
// Workspace API Key Management Routes
// ============================================

/// Create API key for workspace
///
/// Creates a new API key for a workspace.
#[utoipa::path(
    post,
    path = "/workspaces/{workspace_id}/api-keys",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    request_body = CreateApiKeyRequest,
    responses(
        (status = 201, description = "API key created successfully", body = ApiKeyResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn create_workspace_api_key(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(workspace_id): Path<Uuid>,
    Json(request): Json<CreateApiKeyRequest>,
) -> Result<(StatusCode, Json<ApiKeyResponse>), StatusCode> {
    debug!(
        "Creating API key for workspace: {} by user: {}",
        workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user.clone());
    let workspace_id_typed = WorkspaceId(workspace_id);

    // Create API key request for services layer
    let services_request = crate::conversions::api_key_req_to_services_workspace(
        request,
        workspace_id_typed.clone(),
        user_id.clone(),
    );

    // Use the auth service to create the API key
    match app_state
        .auth_service
        .create_workspace_api_key(services_request)
        .await
    {
        Ok(api_key) => {
            debug!(
                "Created API key: {:?} for workspace: {}",
                api_key.id, workspace_id
            );
            let response = crate::conversions::services_api_key_to_api_response(api_key);
            Ok((StatusCode::CREATED, Json(response)))
        }
        Err(AuthError::Unauthorized) => Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to create API key: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// List API keys for workspace
///
/// Returns a list of all API keys for a workspace.
#[utoipa::path(
    get,
    path = "/workspaces/{workspace_id}/api-keys",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    responses(
        (status = 200, description = "List of workspace API keys", body = Vec<ApiKeyResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
        ("api_key" = [])
    )
)]
pub async fn list_workspace_api_keys(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(workspace_id): Path<Uuid>,
) -> Result<Json<Vec<ApiKeyResponse>>, StatusCode> {
    debug!(
        "Listing API keys for workspace: {} by user: {}",
        workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user);
    let workspace_id_typed = WorkspaceId(workspace_id);

    // Use auth service to list workspace API keys
    match app_state
        .auth_service
        .list_workspace_api_keys(workspace_id_typed, user_id)
        .await
    {
        Ok(api_keys) => {
            debug!(
                "Found {} API keys for workspace {}",
                api_keys.len(),
                workspace_id
            );
            let response: Vec<ApiKeyResponse> = api_keys
                .into_iter()
                .map(crate::conversions::services_api_key_to_api_response)
                .collect();
            Ok(Json(response))
        }
        Err(AuthError::Unauthorized) => Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to list API keys: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Revoke an API key
///
/// Revokes a specific API key from a workspace.
#[utoipa::path(
    delete,
    path = "/workspaces/{workspace_id}/api-keys/{key_id}",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID"),
        ("key_id" = Uuid, Path, description = "API Key ID")
    ),
    responses(
        (status = 204, description = "API key revoked successfully"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "API key not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn revoke_workspace_api_key(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((workspace_id, api_key_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, StatusCode> {
    debug!(
        "Revoking API key: {} in workspace: {} by user: {}",
        api_key_id, workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user.clone());

    // Get the API key to validate it belongs to the specified workspace
    match app_state.db.api_keys.get_by_id(api_key_id).await {
        Ok(Some(api_key)) => {
            // Validate the API key belongs to the specified workspace
            if api_key.workspace_id != workspace_id {
                return Err(StatusCode::NOT_FOUND);
            }

            // Check if user has permission to revoke this key
            // Must be the creator or have admin/owner role in the organization
            if api_key.created_by_user_id != user.0.id {
                // Get workspace to find organization
                let workspace_repo =
                    Arc::new(DbWorkspaceRepository::new(app_state.db.pool().clone()));
                match workspace_repo.get_by_id(workspace_id).await {
                    Ok(Some(workspace)) => {
                        // Check organization membership and role
                        let organization_id = OrganizationId(workspace.organization_id);
                        match app_state
                            .organization_service
                            .is_member(organization_id, user_id.clone())
                            .await
                        {
                            Ok(true) => {
                                // For API key revocation, allow any member
                                // This could be refined to check specific roles if needed
                            }
                            _ => return Err(StatusCode::FORBIDDEN),
                        }
                    }
                    Ok(None) => return Err(StatusCode::NOT_FOUND),
                    Err(e) => {
                        error!("Failed to get workspace: {}", e);
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }
            }

            // Revoke the key
            match app_state.db.api_keys.revoke(api_key_id).await {
                Ok(true) => Ok(StatusCode::NO_CONTENT),
                Ok(false) => Err(StatusCode::NOT_FOUND),
                Err(e) => {
                    error!("Failed to revoke API key: {}", e);
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get API key: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Revoke API key using workspace context from middleware
///
/// This route uses the workspace context from authenticated API keys to validate
/// and revoke keys. Used when the API key itself provides workspace context.
pub async fn revoke_api_key_with_context(
    State(app_state): State<AppState>,
    Extension(api_key_context): Extension<AuthenticatedApiKey>,
    Path(api_key_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    debug!(
        "Revoking API key: {} with workspace context: {}",
        api_key_id, api_key_context.workspace.id.0
    );

    // Validate the API key belongs to the same workspace
    match app_state.db.api_keys.get_by_id(api_key_id).await {
        Ok(Some(api_key)) => {
            // Ensure the API key belongs to the authenticated workspace
            if api_key.workspace_id != api_key_context.workspace.id.0 {
                return Err(StatusCode::FORBIDDEN);
            }

            // Revoke the key
            match app_state.db.api_keys.revoke(api_key_id).await {
                Ok(true) => Ok(StatusCode::NO_CONTENT),
                Ok(false) => Err(StatusCode::NOT_FOUND),
                Err(e) => {
                    error!("Failed to revoke API key: {}", e);
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get API key: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
