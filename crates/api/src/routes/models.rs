use crate::models::{
    DecimalPrice, ErrorResponse, ModelListResponse, ModelMetadata, ModelWithPricing,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::Deserialize;
use services::models::ModelsServiceTrait;
use std::sync::Arc;
use tracing::{debug, error};
use utoipa::IntoParams;

#[derive(Clone)]
pub struct ModelsAppState {
    pub models_service: Arc<dyn ModelsServiceTrait + Send + Sync>,
}

/// Query parameters for model listing
#[derive(Debug, Deserialize, IntoParams)]
pub struct ModelListQuery {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// List models with pricing
///
/// Get all available models with pricing information. Public endpoint.
#[utoipa::path(
    get,
    path = "/v1/model/list",
    tag = "Models",
    params(ModelListQuery),
    responses(
        (status = 200, description = "List of models with pricing", body = ModelListResponse),
        (status = 400, description = "Invalid pagination parameters", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    )
)]
pub async fn list_models(
    State(app_state): State<ModelsAppState>,
    Query(query): Query<ModelListQuery>,
) -> Result<ResponseJson<ModelListResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Model list request: limit={}, offset={}",
        query.limit, query.offset
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(query.limit, query.offset)?;

    // Get all models from the service
    let (models, total) = app_state
        .models_service
        .get_models_with_pricing(query.limit, query.offset)
        .await
        .map_err(|_| {
            error!("Failed to get models");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve models".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    // Convert to API models
    let api_models: Vec<ModelWithPricing> = models
        .iter()
        .map(|model| ModelWithPricing {
            model_id: model.model_name.clone(),
            input_cost_per_token: DecimalPrice {
                amount: model.input_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            output_cost_per_token: DecimalPrice {
                amount: model.output_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            metadata: ModelMetadata {
                verifiable: model.verifiable,
                context_length: model.context_length,
                model_display_name: model.model_display_name.clone(),
                model_description: model.model_description.clone(),
                model_icon: model.model_icon.clone(),
                aliases: model.aliases.clone(),
            },
        })
        .collect();

    let response = ModelListResponse {
        models: api_models,
        total,
        limit: query.limit,
        offset: query.offset,
    };

    Ok(ResponseJson(response))
}

/// Get model details
///
/// Get pricing and metadata for a specific model. URL-encode model names containing slashes. Public endpoint.
#[utoipa::path(
    get,
    path = "/v1/model/{model_name}",
    tag = "Models",
    params(
        ("model_name" = String, Path, description = "Model name (URL-encode if it contains slashes)")
    ),
    responses(
        (status = 200, description = "Model details with pricing", body = ModelWithPricing),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    )
)]
pub async fn get_model_by_name(
    State(app_state): State<ModelsAppState>,
    Path(model_name): Path<String>,
) -> Result<ResponseJson<ModelWithPricing>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Get model request for: {}", model_name);

    // Get the model from the service
    let model = app_state
        .models_service
        .resolve_and_get_model(&model_name)
        .await
        .map_err(|e| match e {
            services::models::ModelsError::NotFound(_) => {
                error!("Model not found: '{}' (URL-decoded query)", model_name);
                (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        format!("Model '{model_name}' not found"),
                        "model_not_found".to_string(),
                    )),
                )
            }
            _ => {
                error!("Failed to get model '{}'", model_name);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve model".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    // Convert to API model
    let api_model = ModelWithPricing {
        model_id: model.model_name,
        input_cost_per_token: DecimalPrice {
            amount: model.input_cost_per_token,
            scale: 9,
            currency: "USD".to_string(),
        },
        output_cost_per_token: DecimalPrice {
            amount: model.output_cost_per_token,
            scale: 9,
            currency: "USD".to_string(),
        },
        metadata: ModelMetadata {
            verifiable: model.verifiable,
            context_length: model.context_length,
            model_display_name: model.model_display_name,
            model_description: model.model_description,
            model_icon: model.model_icon,
            aliases: model.aliases,
        },
    };

    Ok(ResponseJson(api_model))
}
