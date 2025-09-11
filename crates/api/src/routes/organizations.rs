use axum::{
    extract::{Json, Path, Query, State, Extension},
    http::StatusCode,
};
use database::{
    Database, Organization, UpdateOrganizationRequest,
    CreateOrganizationRequest, ApiKeyResponse, CreateApiKeyRequest,
};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;
use tracing::{debug, error};
use crate::middleware::AuthenticatedUser;

/// Query parameters for listing
#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 { 20 }

/// Create a new organization
pub async fn create_organization(
    State(db): State<Arc<Database>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<CreateOrganizationRequest>,
) -> Result<Json<Organization>, StatusCode> {
    debug!("Creating organization: {} by user: {}", request.name, user.0.id);
    
    // Any authenticated user can create organizations
    // They will automatically become the owner
    match db.organizations.create(request, user.0.id).await {
        Ok(org) => {
            debug!("Created organization: {} with owner: {}", org.id, user.0.id);
            Ok(Json(org))
        }
        Err(e) => {
            error!("Failed to create organization: {}", e);
            if e.to_string().contains("duplicate key") || e.to_string().contains("already exists") {
                Err(StatusCode::CONFLICT)
            } else {
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

/// Get an organization by ID
pub async fn get_organization(
    State(db): State<Arc<Database>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Organization>, StatusCode> {
    debug!("Getting organization: {} for user: {}", org_id, user.0.id);
    
    // Check if user has access to this organization
    // User must be an organization member
    match db.organizations.get_member(org_id, user.0.id).await {
        Ok(Some(_)) => {
            // User is a member, allow access
        }
        Ok(None) => return Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    
    match db.organizations.get_by_id(org_id).await {
        Ok(Some(org)) => Ok(Json(org)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get organization: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// List organizations
pub async fn list_organizations(
    State(db): State<Arc<Database>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<Organization>>, StatusCode> {
    debug!("Listing organizations for user: {}", user.0.id);
    
    // Users can only see organizations they are members of
    let query = "
        SELECT DISTINCT o.* 
        FROM organizations o
        JOIN organization_members om ON o.id = om.organization_id
        WHERE om.user_id = $1 AND o.is_active = true
        ORDER BY o.created_at DESC
        LIMIT $2 OFFSET $3
    ";
    
    let client = db.pool().get().await.map_err(|e| {
        error!("Failed to get database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    
    let rows = client.query(query, &[&user.0.id, &params.limit, &params.offset]).await.map_err(|e| {
        error!("Failed to query user organizations: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    
    let mut organizations = Vec::new();
    for row in rows {
        if let Ok(Some(org)) = db.organizations.get_by_id(row.get("id")).await {
            organizations.push(org);
        }
    }
    
    Ok(Json(organizations))
}

/// Update an organization
pub async fn update_organization(
    State(db): State<Arc<Database>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<UpdateOrganizationRequest>,
) -> Result<Json<Organization>, StatusCode> {
    debug!("Updating organization: {} by user: {}", org_id, user.0.id);
    
    // Check if user has permission to update this organization
    // User must be an owner or admin of the organization
    match db.organizations.get_member(org_id, user.0.id).await {
        Ok(Some(member)) => {
            if !member.role.can_manage_organization() {
                return Err(StatusCode::FORBIDDEN);
            }
        }
        Ok(None) => return Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    
    match db.organizations.update(org_id, request).await {
        Ok(org) => Ok(Json(org)),
        Err(e) => {
            error!("Failed to update organization: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Delete an organization
pub async fn delete_organization(
    State(db): State<Arc<Database>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    debug!("Deleting organization: {} by user: {}", org_id, user.0.id);
    
    // Only organization owners can delete
    match db.organizations.get_member(org_id, user.0.id).await {
        Ok(Some(member)) => {
            if !member.role.can_delete_organization() {
                return Err(StatusCode::FORBIDDEN);
            }
        }
        Ok(None) => return Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    
    match db.organizations.delete(org_id).await {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to delete organization: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Create an API key for an organization
pub async fn create_organization_api_key(
    State(db): State<Arc<Database>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<CreateApiKeyRequest>,
) -> Result<Json<ApiKeyResponse>, StatusCode> {
    debug!("Creating API key for organization: {} by user: {}", org_id, user.0.id);
    
    // Check if user has permission to create API keys for this organization
    match db.organizations.get_member(org_id, user.0.id).await {
        Ok(Some(member)) => {
            if !member.role.can_manage_api_keys() {
                return Err(StatusCode::FORBIDDEN);
            }
        }
        Ok(None) => return Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    
    match db.api_keys.create(org_id, user.0.id, request).await {
        Ok(api_key) => Ok(Json(api_key)),
        Err(e) => {
            error!("Failed to create API key: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// List API keys for an organization
pub async fn list_organization_api_keys(
    State(db): State<Arc<Database>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<database::ApiKey>>, StatusCode> {
    debug!("Listing API keys for organization: {} for user: {}", org_id, user.0.id);
    
    // Check if user has access to this organization
    match db.organizations.get_member(org_id, user.0.id).await {
        Ok(Some(_)) => {
            // User is a member, allow access
        }
        Ok(None) => return Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    
    match db.api_keys.list_by_organization(org_id).await {
        Ok(keys) => Ok(Json(keys)),
        Err(e) => {
            error!("Failed to list API keys: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}