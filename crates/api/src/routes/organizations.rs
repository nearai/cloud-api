use crate::models::{
    ApiKeyResponse, CreateApiKeyRequest, CreateOrganizationRequest, OrganizationResponse,
    UpdateOrganizationRequest,
};
use crate::{middleware::AuthenticatedUser, routes::api::AppState};
use axum::{
    extract::{Extension, Json, Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use services::{
    auth::AuthError,
    organization::{OrganizationError, OrganizationId},
};
use tracing::{debug, error};
use uuid::Uuid;

/// List all organizations for the authenticated user
pub async fn list_organizations(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<OrganizationResponse>>, StatusCode> {
    debug!("Listing organizations for user: {}", user.0.id);

    // For now, return empty list until we implement the service method
    // TODO: Implement proper organization listing in service layer
    Ok(Json(vec![]))
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

/// Create a new organization
pub async fn create_organization(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<CreateOrganizationRequest>,
) -> Result<Json<OrganizationResponse>, StatusCode> {
    debug!(
        "Creating organization: {} by user: {}",
        request.name, user.0.id
    );

    // Convert API request to services request
    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    match app_state
        .organization_service
        .create_organization(request.name, request.description, user_id.clone())
        .await
    {
        Ok(org) => {
            debug!("Created organization: {} with owner: {}", org.id, user_id.0);
            Ok(Json(crate::conversions::services_org_to_api_org(org)))
        }
        Err(OrganizationError::InvalidParams(msg)) => {
            debug!("Invalid organization creation params: {}", msg);
            Err(StatusCode::BAD_REQUEST)
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

/// Get organization by ID
pub async fn get_organization(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<OrganizationResponse>, StatusCode> {
    debug!("Getting organization: {} by user: {}", org_id, user.0.id);

    let organization_id = OrganizationId(org_id);
    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    // Check if user is a member or can access the organization
    match app_state
        .organization_service
        .is_member(organization_id.clone(), user_id)
        .await
    {
        Ok(true) => {
            // User is a member, get the organization
            match app_state
                .organization_service
                .get_organization(organization_id)
                .await
            {
                Ok(org) => Ok(Json(crate::conversions::services_org_to_api_org(org))),
                Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
                Err(e) => {
                    error!("Failed to get organization: {}", e);
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

/// Update organization
pub async fn update_organization(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<UpdateOrganizationRequest>,
) -> Result<Json<OrganizationResponse>, StatusCode> {
    debug!("Updating organization: {} by user: {}", org_id, user.0.id);

    let organization_id = OrganizationId(org_id);
    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    match app_state
        .organization_service
        .update_organization(
            organization_id,
            user_id,
            request.display_name,
            request.description,
            request.rate_limit,
            request.settings,
        )
        .await
    {
        Ok(updated_org) => Ok(Json(crate::conversions::services_org_to_api_org(
            updated_org,
        ))),
        Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
        Err(OrganizationError::Unauthorized(_)) => Err(StatusCode::FORBIDDEN),
        Err(OrganizationError::InvalidParams(_)) => Err(StatusCode::BAD_REQUEST),
        Err(e) => {
            error!("Failed to update organization: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Delete organization (owner only)
pub async fn delete_organization(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    debug!("Deleting organization: {} by user: {}", org_id, user.0.id);

    let organization_id = OrganizationId(org_id);
    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    match app_state
        .organization_service
        .delete_organization(organization_id, user_id)
        .await
    {
        Ok(true) => {
            debug!("Organization {} deleted successfully", org_id);
            Ok(Json(serde_json::json!({
                "id": org_id.to_string(),
                "deleted": true
            })))
        }
        Ok(false) => {
            error!("Failed to delete organization {}", org_id);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
        Err(OrganizationError::Unauthorized(_)) => Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to delete organization: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Create API key for organization
pub async fn create_organization_api_key(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<CreateApiKeyRequest>,
) -> Result<Json<ApiKeyResponse>, StatusCode> {
    debug!(
        "Creating API key for organization: {} by user: {}",
        org_id, user.0.id
    );

    let organization_id = OrganizationId(org_id);
    let user_id = crate::conversions::authenticated_user_to_user_id(user);
    let services_request = crate::conversions::api_key_req_to_services(
        request,
        organization_id.clone(),
        user_id.clone(),
    );

    match app_state
        .auth_service
        .create_organization_api_key(organization_id, user_id, services_request)
        .await
    {
        Ok(api_key) => {
            debug!("Created API key: {:?}", api_key.id);
            Ok(Json(crate::conversions::services_api_key_to_api_response(
                api_key,
            )))
        }
        Err(AuthError::Unauthorized) => Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to create API key: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// List API keys for organization
pub async fn list_organization_api_keys(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<ApiKeyResponse>>, StatusCode> {
    debug!(
        "Listing API keys for organization: {} by user: {}",
        org_id, user.0.id
    );

    let organization_id = OrganizationId(org_id);
    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    match app_state
        .auth_service
        .list_organization_api_keys(organization_id, user_id)
        .await
    {
        Ok(api_keys) => {
            debug!(
                "Found {} API keys for organization {}",
                api_keys.len(),
                org_id
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
