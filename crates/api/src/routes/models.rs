use crate::models::{
    DecimalPrice, ErrorResponse, ModelArchitecture, ModelListResponse, ModelMetadata,
    ModelWithPricing,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::Deserialize;
use services::models::ModelsServiceTrait;
use std::sync::Arc;
use tracing::{debug, error, warn};
use utoipa::IntoParams;

#[derive(Clone)]
pub struct ModelsAppState {
    pub models_service: Arc<dyn ModelsServiceTrait + Send + Sync>,
}

/// Query parameters for model listing.
///
/// Both fields are optional. Pagination is applied to a short-lived
/// in-process cache of the full model catalog, so successive pages
/// are consistent within the cache TTL window and DB load does not
/// scale with caller pagination.
#[derive(Debug, Deserialize, IntoParams)]
pub struct ModelListQuery {
    /// Maximum number of models to return. Defaults to 100. Must be
    /// non-negative; values are capped only by the catalog size.
    pub limit: Option<i64>,
    /// Number of models to skip from the start of the catalog.
    /// Defaults to 0. Must be non-negative.
    pub offset: Option<i64>,
}

/// List models with pricing
///
/// Get all available models with pricing information. Public endpoint.
///
/// The full model catalog (a few dozen entries) is loaded once and cached
/// in-process for a short TTL. `limit` / `offset` slice the cached list
/// in memory, so pagination is consistent across pages within a single
/// cache window and adds essentially no DB load.
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
    let limit = query.limit.unwrap_or(100);
    let offset = query.offset.unwrap_or(0);

    debug!("Model list request: limit={}, offset={}", limit, offset);

    // Reject negative values; an upper bound is unnecessary because the
    // catalog is small and slicing is bounded by Vec length.
    if limit < 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Limit must be non-negative".to_string(),
                "invalid_parameter".to_string(),
            )),
        ));
    }
    if offset < 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Offset must be non-negative".to_string(),
                "invalid_parameter".to_string(),
            )),
        ));
    }

    // Get all models from the service (served from in-process cache when warm).
    let all_models = app_state
        .models_service
        .get_models_with_pricing()
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

    let total = all_models.len() as i64;
    let offset_usize = offset as usize;
    let limit_usize = limit as usize;

    // Convert to API models, slicing the cached list in memory. This is
    // sub-microsecond for the ~few-dozen-element catalog.
    let api_models: Vec<ModelWithPricing> = all_models
        .into_iter()
        .skip(offset_usize)
        .take(limit_usize)
        .map(|model| ModelWithPricing {
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
            cost_per_image: DecimalPrice {
                amount: model.cost_per_image,
                scale: 9,
                currency: "USD".to_string(),
            },
            cache_read_cost_per_token: model.cache_read_cost_per_token.map(|amount| DecimalPrice {
                amount,
                scale: 9,
                currency: "USD".to_string(),
            }),
            metadata: ModelMetadata {
                verifiable: model.verifiable,
                context_length: model.context_length,
                model_display_name: model.model_display_name,
                model_description: model.model_description,
                model_icon: model.model_icon,
                owned_by: model.owned_by,
                aliases: model.aliases,
                provider_type: model.provider_type,
                provider_config: crate::routes::common::redact_provider_config(
                    model.provider_config,
                ),
                attestation_supported: model.attestation_supported,
                architecture: ModelArchitecture::from_options(
                    model.input_modalities,
                    model.output_modalities,
                ),
                inference_url: None, // Redacted: internal infrastructure URL, admin-only
                hugging_face_id: model.hugging_face_id,
                quantization: model.quantization,
                max_output_length: model.max_output_length,
                supported_sampling_parameters: model.supported_sampling_parameters,
                supported_features: model.supported_features,
                datacenters: crate::models::Datacenter::from_codes(model.datacenters),
                is_ready: model.is_ready,
                deprecation_date: model
                    .deprecation_date
                    .as_ref()
                    .map(crate::routes::admin::format_deprecation_date),
                openrouter_slug: model.openrouter_slug,
            },
        })
        .collect();

    let response = ModelListResponse {
        models: api_models,
        limit,
        offset,
        total,
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
                // Routine 404 on a public, unauthenticated endpoint fed by arbitrary
                // client input (scanners probe slug permutations) — not operational.
                warn!("Model not found: '{}' (URL-decoded query)", model_name);
                (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        format!("Model '{model_name}' not found"),
                        "model_not_found".to_string(),
                    )),
                )
            }
            other => {
                error!(error = %other, "Failed to get model '{}'", model_name);
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
        cost_per_image: DecimalPrice {
            amount: model.cost_per_image,
            scale: 9,
            currency: "USD".to_string(),
        },
        cache_read_cost_per_token: model.cache_read_cost_per_token.map(|amount| DecimalPrice {
            amount,
            scale: 9,
            currency: "USD".to_string(),
        }),
        metadata: ModelMetadata {
            verifiable: model.verifiable,
            context_length: model.context_length,
            model_display_name: model.model_display_name,
            model_description: model.model_description,
            model_icon: model.model_icon,
            owned_by: model.owned_by,
            aliases: model.aliases,
            provider_type: model.provider_type,
            provider_config: crate::routes::common::redact_provider_config(model.provider_config),
            attestation_supported: model.attestation_supported,
            architecture: ModelArchitecture::from_options(
                model.input_modalities,
                model.output_modalities,
            ),
            inference_url: None, // Redacted: internal infrastructure URL, admin-only
            hugging_face_id: model.hugging_face_id,
            quantization: model.quantization,
            max_output_length: model.max_output_length,
            supported_sampling_parameters: model.supported_sampling_parameters,
            supported_features: model.supported_features,
            datacenters: crate::models::Datacenter::from_codes(model.datacenters),
            is_ready: model.is_ready,
            deprecation_date: model
                .deprecation_date
                .as_ref()
                .map(crate::routes::admin::format_deprecation_date),
            openrouter_slug: model.openrouter_slug,
        },
    };

    Ok(ResponseJson(api_model))
}
