use crate::models::{
    DecimalPrice, ErrorResponse, ModelListResponse, ModelMetadata, ModelWithPricing,
};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::Deserialize;
use services::models::ModelsService;
use std::sync::Arc;
use tracing::{debug, error};
use utoipa::IntoParams;

#[derive(Clone)]
pub struct ModelsAppState {
    pub models_service: Arc<dyn ModelsService + Send + Sync>,
}

/// Query parameters for model listing
#[derive(Debug, Deserialize, IntoParams)]
pub struct ModelListQuery {
    /// Page number (1-based)
    #[serde(default = "default_page")]
    pub page: usize,
    /// Number of models per page
    #[serde(default = "default_page_size")]
    pub page_size: usize,
}

fn default_page() -> usize {
    1
}

fn default_page_size() -> usize {
    12
}

/// List all models with pricing information
///
/// Returns a paginated list of all active models with their pricing and metadata information.
/// This is a public endpoint that does not require authentication.
#[utoipa::path(
    get,
    path = "/model/list",
    tag = "Models",
    params(ModelListQuery),
    responses(
        (status = 200, description = "List of models with pricing", body = ModelListResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn list_models(
    State(app_state): State<ModelsAppState>,
    Query(query): Query<ModelListQuery>,
) -> Result<ResponseJson<ModelListResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Model list request: page={}, page_size={}",
        query.page, query.page_size
    );

    // Get all models from the service
    let models = app_state
        .models_service
        .get_models_with_pricing()
        .await
        .map_err(|e| {
            error!("Failed to get models: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve models".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    let total_models = models.len();
    let total_pages = (total_models + query.page_size - 1) / query.page_size;

    // Handle pagination
    let start_index = (query.page.saturating_sub(1)) * query.page_size;
    let end_index = std::cmp::min(start_index + query.page_size, total_models);

    let paginated_models = if start_index < total_models {
        &models[start_index..end_index]
    } else {
        &[]
    };

    // Convert to API models
    let api_models: Vec<ModelWithPricing> = paginated_models
        .iter()
        .map(|model| ModelWithPricing {
            model_id: model.id.to_string(),
            input_cost_per_token: DecimalPrice {
                amount: model.input_cost_amount,
                scale: model.input_cost_scale,
                currency: model.input_cost_currency.clone(),
            },
            output_cost_per_token: DecimalPrice {
                amount: model.output_cost_amount,
                scale: model.output_cost_scale,
                currency: model.output_cost_currency.clone(),
            },
            metadata: ModelMetadata {
                verifiable: model.verifiable,
                context_length: model.context_length,
                model_display_name: model.model_display_name.clone(),
                model_description: model.model_description.clone(),
                model_icon: model.model_icon.clone(),
            },
        })
        .collect();

    let response = ModelListResponse {
        models: api_models,
        total_models,
        page: query.page,
        page_size: query.page_size,
        total_pages: if total_pages == 0 { 1 } else { total_pages },
    };

    Ok(ResponseJson(response))
}
