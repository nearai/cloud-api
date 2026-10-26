//! Administrator-managed access to unlisted models.
//!
//! An unlisted model (`isListed: false`) is restricted: only an organization
//! that holds a grant may use it, and everyone else is answered as if the
//! model did not exist. A grant is an (organization, model) pair. Grants on a
//! listed model have no effect, so access can be prepared before the model is
//! unlisted. A grant counts only while the organization and the model are
//! both active.
//!
//! A grant or a revocation applies at once on the instance that served the
//! request. Other instances follow within the model-resolve cache TTL (30
//! seconds). Each one that changes something is recorded in
//! `organization_model_access_log`.
use crate::{middleware::AdminUser, models::ErrorResponse, routes::admin::AdminAppState};
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use services::admin::{AdminError, ModelAccessGrant, OrganizationModelAccessGrant};
use tracing::{debug, error};
use utoipa::ToSchema;
use uuid::Uuid;

/// A model and the organizations that hold a grant for it.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelAccessResponse {
    /// The model's id.
    pub id: Uuid,
    /// The model's canonical name.
    pub model_name: String,
    /// `false`: the model is restricted to the organizations below. `true`:
    /// every organization may use it and the grants have no effect.
    pub is_listed: bool,
    /// Grants count only while the model is active.
    pub is_active: bool,
    /// The active organizations that hold a grant, oldest grant first.
    pub organizations: Vec<ModelAccessGrantResponse>,
}

/// One organization's grant for a model.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelAccessGrantResponse {
    pub organization_id: Uuid,
    pub organization_name: String,
    pub granted_at: DateTime<Utc>,
    /// The admin who made the grant.
    pub granted_by_user_id: Option<Uuid>,
    pub granted_by_user_email: Option<String>,
}

impl From<ModelAccessGrant> for ModelAccessGrantResponse {
    fn from(grant: ModelAccessGrant) -> Self {
        Self {
            organization_id: grant.organization_id,
            organization_name: grant.organization_name,
            granted_at: grant.granted_at,
            granted_by_user_id: grant.granted_by_user_id,
            granted_by_user_email: grant.granted_by_user_email,
        }
    }
}

/// The models an organization holds a grant for.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationModelAccessResponse {
    pub organization_id: Uuid,
    /// Ordered by model name.
    pub models: Vec<OrganizationModelAccessEntry>,
}

/// One model an organization holds a grant for.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationModelAccessEntry {
    /// The model's id.
    pub id: Uuid,
    /// The model's canonical name.
    pub model_name: String,
    /// `true`: every organization may use the model, so this grant has no
    /// effect until the model is unlisted.
    pub is_listed: bool,
    /// The grant counts only while the model is active.
    pub is_active: bool,
    pub granted_at: DateTime<Utc>,
    /// The admin who made the grant.
    pub granted_by_user_id: Option<Uuid>,
    pub granted_by_user_email: Option<String>,
}

impl From<OrganizationModelAccessGrant> for OrganizationModelAccessEntry {
    fn from(grant: OrganizationModelAccessGrant) -> Self {
        Self {
            id: grant.model.id,
            model_name: grant.model.model_name,
            is_listed: grant.model.is_listed,
            is_active: grant.model.is_active,
            granted_at: grant.granted_at,
            granted_by_user_id: grant.granted_by_user_id,
            granted_by_user_email: grant.granted_by_user_email,
        }
    }
}

type ApiError = (StatusCode, Json<ErrorResponse>);

fn parse_organization_id(org_id: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "Invalid organization ID format".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })
}

fn api_error(error: AdminError) -> ApiError {
    match error {
        AdminError::ModelNotFound(message) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(message, "model_not_found".to_string())),
        ),
        AdminError::OrganizationNotFound(message) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                message,
                "organization_not_found".to_string(),
            )),
        ),
        other => {
            // Discriminant only: the message can carry database error text.
            error!(
                error_kind = ?std::mem::discriminant(&other),
                "Model access operation failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to read or change model access".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        }
    }
}

/// List the organizations that may use a model (Admin only)
///
/// Returns the model's id, name and `isListed`, and the active organizations
/// that hold a grant for it. The grants restrict the model only while it is
/// unlisted; on a listed model they are kept but have no effect.
///
/// **Note:** Model names containing forward slashes must be URL-encoded
/// (e.g. "vendor%2Fmodel").
#[utoipa::path(
    get,
    path = "/v1/admin/models/{model_name}/access",
    tag = "Admin",
    params(
        ("model_name" = String, Path, description = "Canonical model name (URL-encode if it contains slashes)")
    ),
    responses(
        (status = 200, description = "The model and its grants", body = ModelAccessResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden: not an admin", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_model_access(
    State(app_state): State<AdminAppState>,
    Path(model_name): Path<String>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<Json<ModelAccessResponse>, ApiError> {
    debug!("Get model access request for: {}", model_name);

    let access = app_state
        .admin_service
        .get_model_access(&model_name)
        .await
        .map_err(api_error)?;

    Ok(Json(ModelAccessResponse {
        id: access.model.id,
        model_name: access.model.model_name,
        is_listed: access.model.is_listed,
        is_active: access.model.is_active,
        organizations: access.grants.into_iter().map(Into::into).collect(),
    }))
}

/// Grant an organization access to a model (Admin only)
///
/// Idempotent: granting again returns the existing grant unchanged. The grant
/// takes effect when the model is unlisted; while the model is listed every
/// organization may use it anyway. It applies at once on the instance that
/// served this request and within 30 seconds on the others.
///
/// **Note:** Model names containing forward slashes must be URL-encoded
/// (e.g. "vendor%2Fmodel").
#[utoipa::path(
    put,
    path = "/v1/admin/models/{model_name}/access/{org_id}",
    tag = "Admin",
    params(
        ("model_name" = String, Path, description = "Canonical model name (URL-encode if it contains slashes)"),
        ("org_id" = String, Path, description = "The organization's ID (as a UUID)")
    ),
    responses(
        (status = 200, description = "The grant", body = ModelAccessGrantResponse),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden: not an admin, or a read-only admin access token", body = ErrorResponse),
        (status = 404, description = "Model or organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn grant_model_access(
    State(app_state): State<AdminAppState>,
    Path((model_name, org_id)): Path<(String, String)>,
    Extension(admin_user): Extension<AdminUser>,
) -> Result<Json<ModelAccessGrantResponse>, ApiError> {
    debug!(
        "Grant model access request: model={}, org_id={}",
        model_name, org_id
    );
    let organization_id = parse_organization_id(&org_id)?;

    let grant = app_state
        .admin_service
        .grant_model_access(&model_name, organization_id, Some(admin_user.0.id))
        .await
        .map_err(api_error)?;

    Ok(Json(grant.into()))
}

/// Revoke an organization's access to a model (Admin only)
///
/// Idempotent: revoking a grant that does not exist succeeds. If the model is
/// unlisted the organization loses access at once on the instance that served
/// this request and within 30 seconds on the others.
///
/// **Note:** Model names containing forward slashes must be URL-encoded
/// (e.g. "vendor%2Fmodel").
#[utoipa::path(
    delete,
    path = "/v1/admin/models/{model_name}/access/{org_id}",
    tag = "Admin",
    params(
        ("model_name" = String, Path, description = "Canonical model name (URL-encode if it contains slashes)"),
        ("org_id" = String, Path, description = "The organization's ID (as a UUID)")
    ),
    responses(
        (status = 204, description = "The organization holds no grant for the model"),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden: not an admin, or a read-only admin access token", body = ErrorResponse),
        (status = 404, description = "Model or organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn revoke_model_access(
    State(app_state): State<AdminAppState>,
    Path((model_name, org_id)): Path<(String, String)>,
    Extension(admin_user): Extension<AdminUser>,
) -> Result<StatusCode, ApiError> {
    debug!(
        "Revoke model access request: model={}, org_id={}",
        model_name, org_id
    );
    let organization_id = parse_organization_id(&org_id)?;

    app_state
        .admin_service
        .revoke_model_access(&model_name, organization_id, Some(admin_user.0.id))
        .await
        .map_err(api_error)?;

    Ok(StatusCode::NO_CONTENT)
}

/// List the models an organization holds grants for (Admin only)
///
/// Each entry carries the model's `isListed` and `isActive`: a grant restricts
/// nothing while its model is listed and counts for nothing while its model is
/// inactive.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/model-access",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "The organization's ID (as a UUID)")
    ),
    responses(
        (status = 200, description = "The organization's grants", body = OrganizationModelAccessResponse),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden: not an admin", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_organization_model_access(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<Json<OrganizationModelAccessResponse>, ApiError> {
    debug!(
        "List organization model access request for org_id: {}",
        org_id
    );
    let organization_id = parse_organization_id(&org_id)?;

    let grants = app_state
        .admin_service
        .list_organization_model_access(organization_id)
        .await
        .map_err(api_error)?;

    Ok(Json(OrganizationModelAccessResponse {
        organization_id,
        models: grants.into_iter().map(Into::into).collect(),
    }))
}
