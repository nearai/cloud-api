use crate::middleware::AdminUser;
use crate::models::{
    BatchUpdateModelApiRequest, DecimalPrice, ErrorResponse, ModelMetadata,
    ModelPricingHistoryEntry, ModelPricingHistoryResponse, ModelWithPricing, SpendLimit,
    UpdateOrganizationLimitsRequest, UpdateOrganizationLimitsResponse,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use services::admin::{AdminService, UpdateModelAdminRequest};
use std::sync::Arc;
use tracing::{debug, error};

#[derive(Clone)]
pub struct AdminAppState {
    pub admin_service: Arc<dyn AdminService + Send + Sync>,
}

/// Batch upsert models metadata (Admin only)
///
/// Upserts (inserts or updates) pricing and metadata for one or more models. Only authenticated admins can perform this operation.
/// The body should be an array of objects where each key is a model name and the value is the model data.
#[utoipa::path(
    patch,
    path = "/admin/models",
    tag = "Admin",
    request_body = BatchUpdateModelApiRequest,
    responses(
        (status = 200, description = "Models upserted successfully", body = Vec<ModelWithPricing>),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = [])
    )
)]
pub async fn batch_upsert_models(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    ResponseJson(batch_request): ResponseJson<BatchUpdateModelApiRequest>,
) -> Result<ResponseJson<Vec<ModelWithPricing>>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Batch upsert models request with {} model(s)",
        batch_request.len()
    );

    // Validate the batch request format
    if batch_request.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Request body must contain at least one model update".to_string(),
                "invalid_request".to_string(),
            )),
        ));
    }

    // Extract all model updates from the batch request
    let mut models = Vec::new();
    for (model_name, request) in batch_request.iter() {
        let service_request = UpdateModelAdminRequest {
            input_cost_amount: request.input_cost_per_token.as_ref().map(|p| p.amount),
            input_cost_scale: request.input_cost_per_token.as_ref().map(|p| p.scale),
            input_cost_currency: request
                .input_cost_per_token
                .as_ref()
                .map(|p| p.currency.clone()),
            output_cost_amount: request.output_cost_per_token.as_ref().map(|p| p.amount),
            output_cost_scale: request.output_cost_per_token.as_ref().map(|p| p.scale),
            output_cost_currency: request
                .output_cost_per_token
                .as_ref()
                .map(|p| p.currency.clone()),
            model_display_name: request.model_display_name.clone(),
            model_description: request.model_description.clone(),
            model_icon: request.model_icon.clone(),
            context_length: request.context_length,
            verifiable: request.verifiable,
            is_active: request.is_active,
        };
        models.push((model_name.clone(), service_request));
    }

    let updated_models = app_state
        .admin_service
        .batch_upsert_models(models)
        .await
        .map_err(|e| {
            error!("Failed to upsert models: {}", e);
            match e {
                services::admin::AdminError::ModelNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(msg, "model_not_found".to_string())),
                ),
                services::admin::AdminError::InvalidPricing(msg) => (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(msg, "invalid_pricing".to_string())),
                ),
                services::admin::AdminError::Unauthorized(msg) => (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to upsert models".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    // Convert to API response - we need to get model names from the original request
    let mut api_models = Vec::new();
    for (model_index, (model_name, _)) in batch_request.iter().enumerate() {
        let updated_model = &updated_models[model_index];
        api_models.push(ModelWithPricing {
            model_id: model_name.clone(),
            input_cost_per_token: DecimalPrice {
                amount: updated_model.input_cost_amount,
                scale: updated_model.input_cost_scale,
                currency: updated_model.input_cost_currency.clone(),
            },
            output_cost_per_token: DecimalPrice {
                amount: updated_model.output_cost_amount,
                scale: updated_model.output_cost_scale,
                currency: updated_model.output_cost_currency.clone(),
            },
            metadata: ModelMetadata {
                verifiable: updated_model.verifiable,
                context_length: updated_model.context_length,
                model_display_name: updated_model.model_display_name.clone(),
                model_description: updated_model.model_description.clone(),
                model_icon: updated_model.model_icon.clone(),
            },
        });
    }

    Ok(ResponseJson(api_models))
}

/// Get pricing history for a model (Admin only)
///
/// Returns the complete pricing history for a specific model, showing all pricing changes over time.
#[utoipa::path(
    get,
    path = "/admin/models/{model_name}/pricing-history",
    tag = "Admin",
    params(
        ("model_name" = String, Path, description = "Model name to get pricing history for")
    ),
    responses(
        (status = 200, description = "Pricing history retrieved successfully", body = ModelPricingHistoryResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = [])
    )
)]
pub async fn get_model_pricing_history(
    State(app_state): State<AdminAppState>,
    Path(model_name): Path<String>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
) -> Result<ResponseJson<ModelPricingHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Get pricing history request for model: {}", model_name);

    let history = app_state
        .admin_service
        .get_pricing_history(&model_name)
        .await
        .map_err(|e| {
            error!("Failed to get pricing history: {}", e);
            match e {
                services::admin::AdminError::ModelNotFound(_) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        format!("Model '{}' not found", model_name),
                        "model_not_found".to_string(),
                    )),
                ),
                services::admin::AdminError::InvalidPricing(msg) => (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(msg, "invalid_request".to_string())),
                ),
                services::admin::AdminError::Unauthorized(msg) => (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve pricing history".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let history_entries: Vec<ModelPricingHistoryEntry> = history
        .into_iter()
        .map(|h| ModelPricingHistoryEntry {
            id: h.id.to_string(),
            model_id: h.model_id.to_string(),
            input_cost_per_token: DecimalPrice {
                amount: h.input_cost_amount,
                scale: h.input_cost_scale,
                currency: h.input_cost_currency,
            },
            output_cost_per_token: DecimalPrice {
                amount: h.output_cost_amount,
                scale: h.output_cost_scale,
                currency: h.output_cost_currency,
            },
            context_length: h.context_length,
            model_display_name: h.model_display_name,
            model_description: h.model_description,
            effective_from: h.effective_from.to_rfc3339(),
            effective_until: h.effective_until.map(|dt| dt.to_rfc3339()),
            changed_by: h.changed_by,
            change_reason: h.change_reason,
            created_at: h.created_at.to_rfc3339(),
        })
        .collect();

    let response = ModelPricingHistoryResponse {
        model_name,
        history: history_entries,
    };

    Ok(ResponseJson(response))
}

/// Update organization limits (Admin only)
///
/// Updates spending limits for a specific organization. This endpoint is typically called by
/// a billing service with an admin API key when a customer makes a purchase.
#[utoipa::path(
    patch,
    path = "/admin/organizations/{org_id}/limits",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID to update limits for")
    ),
    request_body = UpdateOrganizationLimitsRequest,
    responses(
        (status = 200, description = "Organization limits updated successfully", body = UpdateOrganizationLimitsResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = [])
    )
)]
pub async fn update_organization_limits(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    ResponseJson(request): ResponseJson<UpdateOrganizationLimitsRequest>,
) -> Result<ResponseJson<UpdateOrganizationLimitsResponse>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!(
        "Update organization limits request for org_id: {}, amount: {}, scale: {}, currency: {}",
        org_id, request.spend_limit.amount, request.spend_limit.scale, request.spend_limit.currency
    );

    // Parse organization ID
    let organization_id = uuid::Uuid::parse_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID format".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Convert API request to service request
    let service_request = services::admin::OrganizationLimitsUpdate {
        spend_limit_amount: request.spend_limit.amount,
        spend_limit_scale: request.spend_limit.scale,
        spend_limit_currency: request.spend_limit.currency.clone(),
        changed_by: request.changed_by,
        change_reason: request.change_reason,
    };

    // Update organization limits via admin service
    let updated_limits = app_state
        .admin_service
        .update_organization_limits(organization_id, service_request)
        .await
        .map_err(|e| {
            error!("Failed to update organization limits: {}", e);
            match e {
                services::admin::AdminError::OrganizationNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        msg,
                        "organization_not_found".to_string(),
                    )),
                ),
                services::admin::AdminError::InvalidLimits(msg) => (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(msg, "invalid_limits".to_string())),
                ),
                services::admin::AdminError::Unauthorized(msg) => (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to update organization limits".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    // Convert service response to API response
    let response = UpdateOrganizationLimitsResponse {
        organization_id: updated_limits.organization_id.to_string(),
        spend_limit: SpendLimit {
            amount: updated_limits.spend_limit_amount,
            scale: updated_limits.spend_limit_scale,
            currency: updated_limits.spend_limit_currency,
        },
        updated_at: updated_limits.effective_from.to_rfc3339(),
    };

    Ok(ResponseJson(response))
}
