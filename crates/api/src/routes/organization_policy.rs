use super::organization_credentials::TransferConstraintsSchema;
use crate::middleware::auth::AdminUser;
use crate::middleware::organization_credentials::{error, ApiError};
use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use services::organization_credits::models::{OrganizationPolicy, TransferError};
use services::organization_credits::OrganizationCreditServiceTrait;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRequest {
    policy: OrganizationPolicy,
    reason: String,
}

#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/policy",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization")
    ),
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn get_policy(
    Extension(service): Extension<Arc<dyn OrganizationCreditServiceTrait>>,
    Extension(_admin): Extension<AdminUser>,
    Path(org): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    service
        .configure_policy(org, None)
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    put,
    path = "/v1/admin/organizations/{org_id}/policy",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization")
    ),
    request_body = PolicyRequestSchema,
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn put_policy(
    Extension(service): Extension<Arc<dyn OrganizationCreditServiceTrait>>,
    Extension(admin): Extension<AdminUser>,
    Path(org): Path<Uuid>,
    request: Result<Json<PolicyRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(request) =
        request.map_err(|_| error(TransferError("invalid_policy_configuration").into()))?;

    service
        .configure_policy(org, Some((request.policy, admin.0.id, request.reason)))
        .await
        .map(Json)
        .map_err(error)
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct TransferPolicySchema {
    enabled: bool,
    #[schema(minimum = 1, maximum = 100000, default = 60)]
    max_sends_per_minute: Option<i64>,
    constraints: Option<TransferConstraintsSchema>,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct OrganizationPolicySchema {
    credentials: CredentialPolicySchema,
    credits_transfer: TransferPolicySchema,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct PolicyRequestSchema {
    policy: OrganizationPolicySchema,
    /// Required audit reason, at most 2,000 UTF-8 bytes.
    reason: String,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct CredentialPolicySchema {
    /// Gate owner creation, configuration updates and rotation. Listing/revocation remain available.
    enabled: bool,
}
