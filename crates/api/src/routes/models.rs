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
use tracing::{debug, error};
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
            cache_read_cost_per_token: DecimalPrice {
                amount: model.cache_read_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
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

    let model = app_state
        .models_service
        .resolve_public_model(&model_name)
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
        cost_per_image: DecimalPrice {
            amount: model.cost_per_image,
            scale: 9,
            currency: "USD".to_string(),
        },
        cache_read_cost_per_token: DecimalPrice {
            amount: model.cache_read_cost_per_token,
            scale: 9,
            currency: "USD".to_string(),
        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use services::models::{ModelInfo as ServiceModelInfo, ModelsError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    #[derive(Default)]
    struct PublicResolverService {
        public_calls: AtomicUsize,
        db_calls: AtomicUsize,
    }

    fn service_model(
        model_name: &str,
        max_output_length: Option<i32>,
    ) -> services::models::ModelWithPricing {
        services::models::ModelWithPricing {
            id: Uuid::new_v4(),
            model_name: model_name.to_string(),
            model_display_name: "Public Detail Model".to_string(),
            model_description: "route test model".to_string(),
            model_icon: None,
            input_cost_per_token: 0,
            output_cost_per_token: 0,
            cost_per_image: 0,
            cache_read_cost_per_token: 0,
            context_length: 16_384,
            verifiable: false,
            aliases: vec!["public-alias".to_string()],
            owned_by: "test".to_string(),
            provider_type: "vllm".to_string(),
            provider_config: None,
            attestation_supported: false,
            input_modalities: Some(vec!["text".to_string()]),
            output_modalities: Some(vec!["text".to_string()]),
            inference_url: Some("https://internal.example.test".to_string()),
            hugging_face_id: None,
            quantization: None,
            max_output_length,
            supported_sampling_parameters: vec![],
            supported_features: vec![],
            datacenters: None,
            is_ready: Some(true),
            deprecation_date: None,
            openrouter_slug: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[async_trait]
    impl ModelsServiceTrait for PublicResolverService {
        async fn get_models(&self) -> Result<Vec<ServiceModelInfo>, ModelsError> {
            Ok(Vec::new())
        }

        async fn get_models_with_pricing(
            &self,
        ) -> Result<Vec<services::models::ModelWithPricing>, ModelsError> {
            Ok(Vec::new())
        }

        async fn get_model_by_name(
            &self,
            model_name: &str,
        ) -> Result<services::models::ModelWithPricing, ModelsError> {
            Err(ModelsError::NotFound(format!(
                "Model '{model_name}' not found"
            )))
        }

        async fn resolve_and_get_model(
            &self,
            _identifier: &str,
        ) -> Result<services::models::ModelWithPricing, ModelsError> {
            self.db_calls.fetch_add(1, Ordering::SeqCst);
            Ok(service_model("db-backed/detail", Some(512)))
        }

        async fn resolve_public_model(
            &self,
            identifier: &str,
        ) -> Result<services::models::ModelWithPricing, ModelsError> {
            self.public_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(identifier, "public-alias");
            Ok(service_model("public/enriched-detail", Some(4_096)))
        }

        async fn resolve_alias_cached(&self, _identifier: &str) -> Option<String> {
            None
        }

        async fn get_configured_model_names(&self) -> Result<Vec<String>, ModelsError> {
            Ok(Vec::new())
        }

        async fn invalidate_models_cache(&self) {}
    }

    #[tokio::test]
    async fn get_model_by_name_uses_public_resolver_for_public_detail() {
        let service = Arc::new(PublicResolverService::default());
        let app_state = ModelsAppState {
            models_service: service.clone(),
        };

        let ResponseJson(model) =
            get_model_by_name(State(app_state), Path("public-alias".to_string()))
                .await
                .unwrap();

        assert_eq!(model.model_id, "public/enriched-detail");
        assert_eq!(model.metadata.max_output_length, Some(4_096));
        assert!(model.metadata.inference_url.is_none());
        assert_eq!(service.public_calls.load(Ordering::SeqCst), 1);
        assert_eq!(service.db_calls.load(Ordering::SeqCst), 0);
    }
}
