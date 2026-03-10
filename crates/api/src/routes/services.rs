//! Public GET /v1/services and GET /v1/services/{service_name} — no auth required.
//! Used by chat-api to fetch web_search pricing (cost_per_unit).

use crate::models::{ErrorResponse, ServiceListResponse, ServiceResponse};
use crate::routes::common::{self, validate_limit_offset};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use database::repositories::ServiceRepository;
use services::service_usage::ports::ServiceUnit;
use std::sync::Arc;
use tracing::error;

#[derive(Clone)]
pub struct ServicesRouteState {
    pub service_repository: Arc<ServiceRepository>,
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct ListServicesQueryParams {
    #[serde(default = "common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn service_to_response(
    s: &database::models::Service,
) -> Result<ServiceResponse, (StatusCode, ResponseJson<ErrorResponse>)> {
    let unit = ServiceUnit::try_from(s.unit.as_str()).map_err(|e| {
        // Log internal details server-side, but return a generic error to the client.
        error!("Invalid service unit value in database: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                "Internal server error".to_string(),
                "internal_server_error".to_string(),
            )),
        )
    })?;
    Ok(ServiceResponse {
        id: s.id,
        service_name: s.service_name.clone(),
        display_name: s.display_name.clone(),
        description: s.description.clone(),
        unit,
        cost_per_unit: s.cost_per_unit,
        is_active: s.is_active,
        created_at: s.created_at,
        updated_at: s.updated_at,
    })
}

/// List platform services (public, no auth)
#[utoipa::path(
    get,
    path = "/v1/services",
    tag = "Services",
    params(ListServicesQueryParams),
    responses(
        (status = 200, description = "Services list", body = ServiceListResponse),
        (status = 400, description = "Invalid parameters", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn list_services(
    State(state): State<ServicesRouteState>,
    Query(params): Query<ListServicesQueryParams>,
) -> Result<ResponseJson<ServiceListResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    validate_limit_offset(params.limit, params.offset)?;
    let (services, total) = state
        .service_repository
        // Public endpoint only exposes active services; include_inactive is always false here.
        .list(false, params.limit, params.offset)
        .await
        .map_err(|e| {
            error!("Failed to list services: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve services".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;
    let services_api: Vec<ServiceResponse> = services
        .iter()
        .map(service_to_response)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ResponseJson(ServiceListResponse {
        services: services_api,
        limit: params.limit,
        offset: params.offset,
        total,
    }))
}

/// Get platform service by name (public, no auth)
#[utoipa::path(
    get,
    path = "/v1/services/{service_name}",
    tag = "Services",
    params(("service_name" = String, Path, description = "Service name (e.g. web_search)")),
    responses(
        (status = 200, description = "Service details", body = ServiceResponse),
        (status = 404, description = "Service not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn get_service_by_name(
    State(state): State<ServicesRouteState>,
    Path(service_name): Path<String>,
) -> Result<ResponseJson<ServiceResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let s = state
        .service_repository
        .get_active_by_name(&service_name)
        .await
        .map_err(|e| {
            error!("Failed to get service by name: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve service".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;
    let service = s.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Service not found".to_string(),
                "not_found".to_string(),
            )),
        )
    })?;
    let response = service_to_response(&service)?;
    Ok(ResponseJson(response))
}
