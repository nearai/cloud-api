use crate::middleware::{AdminUser, AuthenticatedUser};
use crate::models::ErrorResponse;
use crate::routes::admin::AdminAppState;
use crate::routes::api::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use serde::{Deserialize, Serialize};
use services::auth::UserId;
use services::organization::OrganizationId;
use services::staking_farm::{OrganizationStakingFarmSource, StakingFarmSourceConflict};
use utoipa::ToSchema;
use uuid::Uuid;

type RouteResult<T> = Result<ResponseJson<T>, (StatusCode, ResponseJson<ErrorResponse>)>;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StakingFarmConfigResponse {
    pub enabled: bool,
    pub network_id: String,
    pub contract_id: String,
    pub farm_product_id: String,
    pub farm_price_id: Option<String>,
    pub credit_nano_usd_per_reward_unit: i64,
    pub sync_staleness_seconds: i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StakingFarmStateResponse {
    pub organization_id: String,
    pub near_account_id: String,
    pub network_id: String,
    pub contract_id: String,
    pub farm_product_id: String,
    pub farm_price_id: Option<String>,
    pub credit_nano_usd_per_reward_unit: i64,
    pub accumulated_reward_units_24: Option<String>,
    pub pending_reward_units_24: Option<String>,
    pub total_earned_reward_units_24: Option<String>,
    pub farm_credit_nano_usd: Option<i64>,
    pub last_synced_reward_units_24: Option<String>,
    pub last_synced_credit_nano_usd: Option<i64>,
    pub last_synced_at: Option<String>,
    pub sync_status: String,
    pub last_sync_error: Option<String>,
    pub active_positions: serde_json::Value,
}

/// Get staking farm configuration
///
/// Returns the active House of Stake farm configuration used to convert
/// on-chain staking reward units into NEAR AI Cloud credits.
#[utoipa::path(
    get,
    path = "/v1/staking/farm/config",
    tag = "Staking Farm",
    responses(
        (status = 200, description = "Staking farm configuration", body = StakingFarmConfigResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_staking_farm_config(
    State(app_state): State<AppState>,
) -> RouteResult<StakingFarmConfigResponse> {
    let config = app_state.staking_farm_service.config();
    Ok(ResponseJson(StakingFarmConfigResponse {
        enabled: config.enabled,
        network_id: config.network_id.clone(),
        contract_id: config.contract_id.clone(),
        farm_product_id: config.farm_product_id.clone(),
        farm_price_id: config.farm_price_id.clone(),
        credit_nano_usd_per_reward_unit: config.credit_nano_usd_per_reward_unit,
        sync_staleness_seconds: config.sync_staleness_seconds,
    }))
}

/// Get organization staking farm state
///
/// Returns the staking farm source and last synced farm-credit state for an
/// organization. The organization must be the NEAR-authenticated user's default
/// organization.
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/staking/farm",
    tag = "Staking Farm",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization staking farm state", body = StakingFarmStateResponse),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "No staking farm source found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_staking_farm(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
) -> RouteResult<StakingFarmStateResponse> {
    let organization_id = parse_uuid(&org_id, "Invalid organization ID")?;
    require_near_default_org(&app_state, &user, organization_id).await?;
    let source = app_state
        .staking_farm_service
        .get_source(organization_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found("No staking farm source found"))?;

    Ok(ResponseJson(source_to_response(source)))
}

/// Sync organization staking farm credits
///
/// Links or refreshes the NEAR-authenticated user's staking farm source for the
/// organization, then syncs reward units from the configured staking contract.
#[utoipa::path(
    post,
    path = "/v1/organizations/{org_id}/staking/farm/sync",
    tag = "Staking Farm",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Synced organization staking farm state", body = StakingFarmStateResponse),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 409, description = "NEAR account is already linked to another organization", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn sync_organization_staking_farm(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
) -> RouteResult<StakingFarmStateResponse> {
    let organization_id = parse_uuid(&org_id, "Invalid organization ID")?;
    require_near_default_org(&app_state, &user, organization_id).await?;
    let near_account_id = user.0.provider_user_id.clone();
    let source = app_state
        .staking_farm_service
        .sync_for_near_account(organization_id, near_account_id, user.0.id)
        .await
        .map_err(staking_farm_error)?;

    Ok(ResponseJson(source_to_response(source)))
}

/// Get admin organization staking farm state
///
/// Returns the staking farm source and last synced farm-credit state for any
/// organization. Requires platform admin access.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/staking/farm",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization staking farm state", body = StakingFarmStateResponse),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "No staking farm source found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_admin_organization_staking_farm(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    Path(org_id): Path<String>,
) -> RouteResult<StakingFarmStateResponse> {
    let organization_id = parse_uuid(&org_id, "Invalid organization ID")?;
    let source = app_state
        .staking_farm_service
        .get_source(organization_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found("No staking farm source found"))?;

    Ok(ResponseJson(source_to_response(source)))
}

/// Sync admin organization staking farm credits
///
/// Refreshes staking farm reward units and derived credits for any linked
/// organization. Requires platform admin access.
#[utoipa::path(
    post,
    path = "/v1/admin/organizations/{org_id}/staking/farm/sync",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Synced organization staking farm state", body = StakingFarmStateResponse),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "No staking farm source found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn sync_admin_organization_staking_farm(
    State(app_state): State<AdminAppState>,
    Extension(admin_user): Extension<AdminUser>,
    Path(org_id): Path<String>,
) -> RouteResult<StakingFarmStateResponse> {
    let organization_id = parse_uuid(&org_id, "Invalid organization ID")?;
    let source = app_state
        .staking_farm_service
        .get_source(organization_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found("No staking farm source found"))?;
    let source = app_state
        .staking_farm_service
        .sync_for_source(source, Some(admin_user.0.id))
        .await
        .map_err(internal_error)?;

    Ok(ResponseJson(source_to_response(source)))
}

async fn require_near_default_org(
    app_state: &AppState,
    user: &AuthenticatedUser,
    organization_id: Uuid,
) -> Result<(), (StatusCode, ResponseJson<ErrorResponse>)> {
    if user.0.auth_provider != "near" || user.0.provider_user_id.is_empty() {
        return Err((
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "Staking farm credits require NEAR wallet authentication".to_string(),
                "near_auth_required".to_string(),
            )),
        ));
    }

    // The organization service returns active memberships in creation order; until
    // users have a designated default-org flag, treat the earliest org as default.
    let orgs = app_state
        .organization_service
        .list_organizations_for_user(UserId(user.0.id), 1, 0, None, None)
        .await
        .map_err(internal_error)?;

    let default_org = orgs
        .first()
        .ok_or_else(|| not_found("No default organization found for user"))?;
    if default_org.id != OrganizationId(organization_id) {
        return Err((
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "Staking farm credits can only be linked to the user's default organization"
                    .to_string(),
                "non_default_organization".to_string(),
            )),
        ));
    }

    Ok(())
}

fn source_to_response(source: OrganizationStakingFarmSource) -> StakingFarmStateResponse {
    StakingFarmStateResponse {
        organization_id: source.organization_id.to_string(),
        near_account_id: source.near_account_id,
        network_id: source.network_id,
        contract_id: source.contract_id,
        farm_product_id: source.farm_product_id,
        farm_price_id: source.farm_price_id,
        credit_nano_usd_per_reward_unit: source.credit_nano_usd_per_reward_unit,
        accumulated_reward_units_24: source.last_synced_accumulated_reward_units_24,
        pending_reward_units_24: source.last_synced_pending_reward_units_24,
        total_earned_reward_units_24: source.last_synced_reward_units_24.clone(),
        farm_credit_nano_usd: source.last_synced_credit_nano_usd,
        last_synced_reward_units_24: source.last_synced_reward_units_24,
        last_synced_credit_nano_usd: source.last_synced_credit_nano_usd,
        last_synced_at: source.last_synced_at.map(|value| value.to_rfc3339()),
        sync_status: source.sync_status,
        last_sync_error: source.last_sync_error,
        active_positions: source.active_positions,
    }
}

fn parse_uuid(
    value: &str,
    message: &str,
) -> Result<Uuid, (StatusCode, ResponseJson<ErrorResponse>)> {
    Uuid::parse_str(value).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                message.to_string(),
                "invalid_id".to_string(),
            )),
        )
    })
}

fn not_found(message: &str) -> (StatusCode, ResponseJson<ErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        ResponseJson(ErrorResponse::new(
            message.to_string(),
            "not_found".to_string(),
        )),
    )
}

fn staking_farm_error(error: anyhow::Error) -> (StatusCode, ResponseJson<ErrorResponse>) {
    if error.downcast_ref::<StakingFarmSourceConflict>().is_some() {
        return (
            StatusCode::CONFLICT,
            ResponseJson(ErrorResponse::new(
                "NEAR account is already linked to another organization".to_string(),
                "staking_farm_source_conflict".to_string(),
            )),
        );
    }

    internal_error(error)
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, ResponseJson<ErrorResponse>) {
    tracing::error!(error = %error, "Staking farm route failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        ResponseJson(ErrorResponse::new(
            "Failed to process staking farm request".to_string(),
            "internal_server_error".to_string(),
        )),
    )
}
