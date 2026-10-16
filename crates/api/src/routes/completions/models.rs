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
/// serve and advertise a model that this API keeps out of its own public
/// catalog.
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

/// Convert a nano-USD amount (DB scale 9 — used for per-token, per-image, and
/// per-request prices) to a USD string suitable for OpenRouter
/// (e.g. 8_000 → "0.000008"). Strings are required by the OpenRouter provider
/// spec to avoid float precision issues, so this uses pure integer arithmetic.
/// Pricing is non-negative throughout the system; a defensive `abs()` keeps
/// the formatter total in the unexpected case.
pub(super) fn nano_dollars_to_per_token_string(nano_dollars: i64) -> String {
    if nano_dollars == 0 {
        return "0".to_string();
    }
    let n = nano_dollars.unsigned_abs();
    let dollars = n / 1_000_000_000;
    let nanos = n % 1_000_000_000;
    let mut s = if nanos == 0 {
        format!("{dollars}")
    } else {
        let frac = format!("{nanos:09}");
        let frac = frac.trim_end_matches('0');
        format!("{dollars}.{frac}")
    };
    if nano_dollars < 0 {
        s.insert(0, '-');
    }
    s
}

pub(super) fn model_with_pricing_to_info(model: services::models::ModelWithPricing) -> ModelInfo {
    // Legacy HuggingFace-style fields: USD per million tokens.
    // nano_dollars_per_token * 0.001 = USD per million.
    let input_per_million = (model.input_cost_per_token as f64) * 0.001;
    let output_per_million = (model.output_cost_per_token as f64) * 0.001;

    let pricing = ModelPricing {
        input: input_per_million,
        output: output_per_million,
        prompt: nano_dollars_to_per_token_string(model.input_cost_per_token),
        completion: nano_dollars_to_per_token_string(model.output_cost_per_token),
        image: nano_dollars_to_per_token_string(model.cost_per_image),
        request: "0".to_string(),
        // None = cache pricing disabled -> omit the field entirely.
        // Some(0) is a real (genuinely free) price and renders as "0".
        input_cache_read: model
            .cache_read_cost_per_token
            .map(nano_dollars_to_per_token_string),
    };

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
