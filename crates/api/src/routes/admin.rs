use crate::middleware::AdminUser;
use crate::models::{
    BatchUpdateModelApiRequest, DecimalPrice, ErrorResponse, ModelMetadata,
    ModelPricingHistoryEntry, ModelPricingHistoryResponse, ModelWithPricing,
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
    for model_map in &batch_request {
        if model_map.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Each entry must contain at least one model".to_string(),
                    "invalid_request".to_string(),
                )),
            ));
        }

        for (model_name, request) in model_map.iter() {
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
    let mut model_index = 0;
    for model_map in &batch_request {
        for (model_name, _) in model_map.iter() {
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
            model_index += 1;
        }
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
