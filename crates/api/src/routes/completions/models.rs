use super::*;

/// List available models
///
/// Returns all AI models available for completions. OpenAI-compatible endpoint.
#[utoipa::path(
    get,
    path = "/v1/models",
    tag = "Chat",
    responses(
        (status = 200, description = "List of available models", body = ModelsResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ()
    )
)]
pub async fn models(
    State(app_state): State<AppState>,
) -> Result<ResponseJson<ModelsResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Models list request");

    let models = app_state
        .models_service
        .get_public_models_with_pricing()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    e.to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    let response = ModelsResponse {
        object: "list".to_string(),
        data: models.into_iter().map(model_with_pricing_to_info).collect(),
    };
    Ok(ResponseJson(response))
}

/// List every active model for trusted services, unlisted models included.
///
/// The document has the shape of `GET /v1/models` and adds the models whose
/// `is_listed` is false. A gateway lane reads its catalog here, so it can
/// serve a model that this API keeps out of its own public catalog. Which
/// organizations may use an unlisted model is in
/// `GET /v1/internal/model-access`.
///
/// Auth: `Authorization: Bearer <CLOUD_API_USAGE_TOKEN>`, the service secret
/// of `POST /v1/internal/usage`. The endpoint answers 503 until that token is
/// configured. Not part of the public OpenAPI document. The answer is marked
/// `no-store`: unlike the public document it must not sit in a shared cache.
pub async fn models_internal(
    State(app_state): State<AppState>,
    headers: header::HeaderMap,
) -> Result<
    (
        [(header::HeaderName, &'static str); 1],
        ResponseJson<ModelsResponse>,
    ),
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    crate::routes::usage::verify_internal_usage_token(
        &headers,
        app_state.config.internal_usage_token.as_deref(),
    )?;

    let models = app_state
        .models_service
        .get_models_with_pricing()
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to get models for the internal catalog");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve models".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        ResponseJson(ModelsResponse {
            object: "list".to_string(),
            data: models.into_iter().map(model_with_pricing_to_info).collect(),
        }),
    ))
}

/// List the restricted models and who may use them, for trusted services.
///
/// An unlisted model is restricted: only organizations that hold a grant may
/// use it. This document lists every active unlisted model with the ids of
/// the active organizations that hold a grant for it (an empty list when
/// there are none). A model that is absent from it is unrestricted. A
/// gateway lane polls it and enforces access itself for the requests it
/// serves. It is read from the database on every call, so a grant or a
/// revocation shows up on the next poll of any instance.
///
/// Auth: `Authorization: Bearer <CLOUD_API_USAGE_TOKEN>`, the service secret
/// of `POST /v1/internal/usage`. The endpoint answers 503 until that token is
/// configured. Not part of the public OpenAPI document. The answer is marked
/// `no-store`: it must not sit in a shared cache.
pub async fn model_access_internal(
    State(app_state): State<AppState>,
    headers: header::HeaderMap,
) -> Result<
    (
        [(header::HeaderName, &'static str); 1],
        ResponseJson<ModelAccessDocument>,
    ),
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    crate::routes::usage::verify_internal_usage_token(
        &headers,
        app_state.config.internal_usage_token.as_deref(),
    )?;

    let restricted = app_state
        .models_service
        .get_restricted_model_access()
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to list model access for the internal document");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve model access".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        ResponseJson(ModelAccessDocument {
            object: "model_access".to_string(),
            models: restricted
                .into_iter()
                .map(|model| ModelAccessDocumentEntry {
                    id: model.model_name,
                    organization_ids: model.organization_ids,
                })
                .collect(),
        }),
    ))
}

pub(super) fn model_with_pricing_to_info(model: services::models::ModelWithPricing) -> ModelInfo {
    let pricing = CatalogPricingAmounts {
        input_cost_per_token: model.input_cost_per_token,
        output_cost_per_token: model.output_cost_per_token,
        cost_per_image: model.cost_per_image,
        cache_read_cost_per_token: model.cache_read_cost_per_token,
    }
    .into();
    let upcoming_pricing = model.upcoming_pricing.map(Into::into);

    // OpenRouter's provider spec marks `input_modalities` / `output_modalities`
    // as REQUIRED fields. They are derived from the nullable `architecture`
    // column, so models whose architecture was never backfilled would otherwise
    // emit no modality fields at all. Default to text/text so the required
    // fields are never absent. Real per-model values are set via the admin API.
    let input_modalities = model
        .input_modalities
        .unwrap_or_else(|| vec!["text".to_string()]);
    let output_modalities = model
        .output_modalities
        .unwrap_or_else(|| vec!["text".to_string()]);
    let architecture = Some(ModelArchitecture {
        input_modalities: input_modalities.clone(),
        output_modalities: output_modalities.clone(),
    });

    let name = if model.model_display_name.is_empty() {
        None
    } else {
        Some(model.model_display_name)
    };
    let description = if model.model_description.is_empty() {
        None
    } else {
        Some(model.model_description)
    };

    ModelInfo {
        id: model.model_name,
        object: "model".to_string(),
        created: model.created_at.timestamp(),
        owned_by: model.owned_by,
        name,
        hugging_face_id: model.hugging_face_id,
        quantization: model.quantization,
        pricing: Some(pricing),
        upcoming_pricing,
        text_pricing: model
            .text_pricing
            .map(|profile| serde_json::to_value(profile).unwrap_or(serde_json::Value::Null)),
        context_length: Some(model.context_length),
        max_output_length: model.max_output_length,
        architecture,
        input_modalities: Some(input_modalities),
        output_modalities: Some(output_modalities),
        supported_sampling_parameters: model.supported_sampling_parameters,
        supported_features: model.supported_features,
        is_ready: model.is_ready,
        deprecation_date: model
            .deprecation_date
            .as_ref()
            .map(crate::routes::admin::format_deprecation_date),
        successor_model_id: model.successor_model_name,
        description,
        top_provider: Some(TopProvider {
            context_length: Some(model.context_length),
            max_completion_tokens: model.max_output_length,
            is_moderated: false,
        }),
        datacenters: crate::models::Datacenter::from_codes(model.datacenters),
        // OpenRouter requires `id` to match its canonical slug, or an explicit
        // override via this nested object. Emit it only when an override is set
        // (NULL/empty → omit the key entirely).
        openrouter: model
            .openrouter_slug
            .filter(|s| !s.is_empty())
            .map(|slug| crate::models::OpenRouter { slug }),
    }
}
