use crate::conversions::{
    api_invitation_email_status_to_services, api_invitation_status_to_services,
    services_invitation_email_delivery_to_api, services_invitation_resend_result_to_api,
};
use crate::middleware::AdminUser;
use crate::models::{
    AdminAccessTokenResponse, AdminInvitationEmailResendResultResponse, AdminModelListResponse,
    AdminModelWithPricing, AdminOrganizationMemberResponse, AdminOrganizationResponse,
    AdminServiceResponse, AdminUserOrganizationDetails, AdminUserResponse,
    BatchUpdateModelApiRequest, CreateAdminAccessTokenRequest, CreateServiceRequest, CreditType,
    DecimalPrice, DecimalPriceRequest, DeleteAdminAccessTokenRequest, DeleteModelRequest,
    DeprecateModelRequest, DeprecateModelResponse, ErrorResponse,
    GetOrganizationConcurrentLimitResponse, ListAdminInvitationEmailDeliveriesResponse,
    ListAdminOrganizationMembersResponse, ListOrganizationsAdminResponse,
    ListPricingChangesResponse, ListUsersResponse, MemberRole, ModelArchitecture,
    ModelDeprecationConfirmResponse, ModelDeprecationPreviewResponse, ModelDeprecationRequest,
    ModelHistoryEntry, ModelHistoryResponse, ModelMetadata, ModelWithPricing,
    OrgLimitsHistoryEntry, OrgLimitsHistoryResponse, OrganizationUsage, PricingChangeBatchRequest,
    PricingChangeConfirmResponse, PricingChangeModelPreviewDto, PricingChangePreviewResponse,
    PricingFieldUpdates, PricingFields, ScheduledPricingChangeDto, SpendLimit,
    UpdateOrganizationConcurrentLimitRequest, UpdateOrganizationConcurrentLimitResponse,
    UpdateOrganizationLimitsRequest, UpdateOrganizationLimitsResponse, UpdateServiceRequest,
};
use crate::routes::common::format_amount;
use crate::routes::usage::{compute_organization_balance_response, OrganizationBalanceResponse};
use axum::{
    extract::{Json, Path, Query, State},
    http::HeaderMap,
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use chrono::{DateTime, Duration, Timelike, Utc};
use config::ApiConfig;
use services::admin::{AdminService, AnalyticsService, UpdateModelAdminRequest};
use services::auth::AuthServiceTrait;
use services::github_dispatch::GitHubDispatcher;
use services::usage::UsageServiceTrait;
use std::sync::Arc;
use tracing::{debug, error, warn, Instrument};
use uuid::Uuid;

/// OpenRouter's fixed `supported_sampling_parameters` vocabulary. Values written
/// via the admin API are validated against this list, and any pinned/seeded
/// catalog row (e.g. the Chutes seed in `crate::ensure_chutes_catalog_row`) must
/// stay a subset of it so `GET /v1/models` only ever emits known values.
/// See migration V0051 and the OpenRouter provider spec.
pub(crate) const VALID_SAMPLING_PARAMS: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "top_a",
    "frequency_penalty",
    "presence_penalty",
    "repetition_penalty",
    "stop",
    "seed",
    "max_tokens",
    "logit_bias",
];

/// OpenRouter's fixed `supported_features` vocabulary. See `VALID_SAMPLING_PARAMS`.
pub(crate) const VALID_FEATURES: &[&str] = &[
    "tools",
    "json_mode",
    "structured_outputs",
    "logprobs",
    "web_search",
    "reasoning",
];

/// Parse an OpenRouter `deprecation_date` into a normalized `DateTime<Utc>`.
///
/// Follows the OpenRouter provider spec
/// (<https://openrouter.ai/docs/guides/community/for-providers>) exactly. The
/// spec models deprecation at *hour* precision and defines two input shapes:
///
/// - A bare ISO 8601 date (e.g. `2030-01-01`). Per the spec: *"Date-only
///   values default to 13:00 UTC on that date."* So we store 13:00 UTC.
/// - An explicit RFC 3339 instant in whole-hour UTC form
///   (e.g. `2025-06-01T15:00:00Z`). The spec only expresses whole UTC hours, so
///   we accept the datetime form **only** when it is already an exact whole-hour
///   UTC instant: minutes, seconds, and sub-second components must all be zero
///   and the offset must be UTC. We do **not** truncate finer-grained values —
///   that would silently move a model's deprecation earlier than requested and
///   accept inputs outside the advertised contract. Anything off the hour, or in
///   a non-UTC offset, returns `None` so the caller rejects it with a 400.
///
/// Note: a zero offset spelled `+00:00` is treated as equivalent to `Z` (both
/// denote the same UTC instant), provided minutes/seconds/sub-second are zero.
///
/// Returns `None` for anything that does not parse as a date or as a whole-hour
/// UTC RFC 3339 instant, so callers can reject it with a 400.
fn parse_deprecation_date(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        // Reject non-UTC offsets — the contract is whole-hour UTC only.
        if dt.offset().local_minus_utc() != 0 {
            return None;
        }
        let utc = dt.with_timezone(&Utc);
        // Reject anything not already on the top of the hour. We do not truncate;
        // off-hour datetimes are invalid input.
        if utc.minute() != 0 || utc.second() != 0 || utc.nanosecond() != 0 {
            return None;
        }
        return Some(utc);
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        // Date-only values default to 13:00 UTC on that date (OpenRouter spec).
        return date
            .and_hms_opt(13, 0, 0)
            .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    None
}

/// Serialize a stored `deprecation_date` into the OpenRouter-compatible
/// UTC-hour form `YYYY-MM-DDTHH:00:00Z` (matching the spec's examples, e.g.
/// `2025-06-01T15:00:00Z`). The stored value is always normalized to a whole
/// UTC hour by [`parse_deprecation_date`], so this is a faithful round-trip.
pub(crate) fn format_deprecation_date(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:00:00Z").to_string()
}

/// Validate an OpenRouter `openrouter.slug` override.
///
/// OpenRouter's `/api/v1/models` ids are lowercase `author/slug` pairs (e.g.
/// `z-ai/glm-5.1`, `qwen/qwen3.6-27b`). We accept exactly that shape: one `/`
/// separator, each segment starting and ending with `[a-z0-9]` and containing
/// only `[a-z0-9._-]` in between. This mirrors the regex
/// `^[a-z0-9](?:[a-z0-9._-]*[a-z0-9])?/[a-z0-9](?:[a-z0-9._-]*[a-z0-9])?$`
/// without pulling in a regex dependency.
pub(crate) fn is_valid_openrouter_slug(slug: &str) -> bool {
    fn is_valid_segment(seg: &str) -> bool {
        let bytes = seg.as_bytes();
        if bytes.is_empty() {
            return false;
        }
        let is_boundary = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let is_interior = |b: u8| is_boundary(b) || matches!(b, b'.' | b'_' | b'-');
        // First and last must be boundary chars; interior may use . _ -
        if !is_boundary(bytes[0]) || !is_boundary(bytes[bytes.len() - 1]) {
            return false;
        }
        bytes.iter().all(|&b| is_interior(b))
    }

    let mut parts = slug.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        // Exactly two non-empty, well-formed segments (no second `/`).
        (Some(author), Some(name), None) => is_valid_segment(author) && is_valid_segment(name),
        _ => false,
    }
}

#[derive(Clone)]
pub struct AdminAppState {
    pub admin_service: Arc<dyn AdminService + Send + Sync>,
    pub analytics_service: Arc<AnalyticsService>,
    pub organization_service:
        Arc<dyn services::organization::OrganizationServiceTrait + Send + Sync>,
    pub auth_service: Arc<dyn AuthServiceTrait>,
    pub usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    pub config: Arc<ApiConfig>,
    pub admin_access_token_repository: Arc<database::repositories::AdminAccessTokenRepository>,
    pub inference_provider_pool: Arc<services::inference_provider_pool::InferenceProviderPool>,
    pub github_dispatcher: Arc<dyn GitHubDispatcher>,
    pub infra_service: Arc<services::admin::InfraService>,
}

/// Small helper for 400 responses from analytics query-param validation.
fn bad_request(
    message: impl Into<String>,
    code: &str,
) -> (StatusCode, ResponseJson<ErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        ResponseJson(ErrorResponse::new(message.into(), code.to_string())),
    )
}

/// Batch upsert models metadata (Admin only)
///
/// Upserts (inserts or updates) pricing and metadata for one or more models. Only authenticated admins can perform this operation.
/// The body should be an array of objects where each key is a model name and the value is the model data.
#[utoipa::path(
    patch,
    path = "/v1/admin/models",
    tag = "Admin",
    request_body = BatchUpdateModelApiRequest,
    responses(
        (status = 200, description = "Models upserted successfully", body = Vec<ModelWithPricing>),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn batch_upsert_models(
    State(app_state): State<AdminAppState>,
    Extension(admin_user): Extension<AdminUser>, // Require admin auth
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

    // Validate all pricing fields are non-negative to prevent incorrect billing
    for (model_name, request) in &batch_request {
        let validate_price = |price: &Option<DecimalPriceRequest>, field: &str| {
            if let Some(p) = price {
                p.validate().map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            format!("model '{model_name}': {field}: {e}"),
                            "invalid_request".to_string(),
                        )),
                    )
                })?;
            }
            Ok::<(), (StatusCode, ResponseJson<ErrorResponse>)>(())
        };
        validate_price(&request.input_cost_per_token, "inputCostPerToken")?;
        validate_price(&request.output_cost_per_token, "outputCostPerToken")?;
        validate_price(&request.cost_per_image, "costPerImage")?;
        validate_price(&request.cache_read_cost_per_token, "cacheReadCostPerToken")?;

        // OpenRouter vocabulary checks. The provider spec at
        // https://openrouter.ai/docs/guides/community/for-providers enumerates
        // valid values for each field; rejecting unknowns at the write path
        // keeps `GET /v1/models` honest, since these flow into the catalog
        // OpenRouter consumes.
        if let Some(q) = &request.quantization {
            const VALID_QUANTIZATIONS: &[&str] =
                &["int4", "int8", "fp4", "fp6", "fp8", "fp16", "bf16", "fp32"];
            if !VALID_QUANTIZATIONS.contains(&q.as_str()) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!(
                            "model '{model_name}': quantization: '{q}' is not in OpenRouter's vocabulary ({})",
                            VALID_QUANTIZATIONS.join(", ")
                        ),
                        "invalid_request".to_string(),
                    )),
                ));
            }
        }
        if let Some(max_out) = request.max_output_length {
            if max_out <= 0 {
                return Err((
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!("model '{model_name}': maxOutputLength must be positive"),
                        "invalid_request".to_string(),
                    )),
                ));
            }
        }
        if let Some(params) = &request.supported_sampling_parameters {
            for p in params {
                if !VALID_SAMPLING_PARAMS.contains(&p.as_str()) {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            format!(
                                "model '{model_name}': supportedSamplingParameters: '{p}' is not in OpenRouter's vocabulary"
                            ),
                            "invalid_request".to_string(),
                        )),
                    ));
                }
            }
        }
        if let Some(features) = &request.supported_features {
            for f in features {
                if !VALID_FEATURES.contains(&f.as_str()) {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            format!(
                                "model '{model_name}': supportedFeatures: '{f}' is not in OpenRouter's vocabulary"
                            ),
                            "invalid_request".to_string(),
                        )),
                    ));
                }
            }
        }
        if let Some(datacenters) = &request.datacenters {
            // OpenRouter's `datacenters` country_code is an ISO 3166 Alpha-2
            // code: exactly two ASCII uppercase letters. Reject anything else
            // so the catalog can't emit malformed codes.
            for dc in datacenters {
                let code = &dc.country_code;
                let valid = code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase());
                if !valid {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            format!(
                                "model '{model_name}': datacenters: '{code}' is not a 2-letter uppercase ISO 3166 Alpha-2 country code"
                            ),
                            "invalid_request".to_string(),
                        )),
                    ));
                }
            }
        }
        // `deprecation_date` must be either a bare date (`YYYY-MM-DD`, which
        // defaults to 13:00 UTC) or a whole-hour UTC instant
        // (`YYYY-MM-DDTHH:00:00Z`). We reject off-hour or non-UTC datetimes
        // rather than silently truncating them, so the stored value — and the
        // `GET /v1/models` we serve from it — never deprecates a model earlier
        // than requested. An explicit `null` (clear) and an omitted field both
        // skip this check.
        if let Some(Some(d)) = &request.deprecation_date {
            if parse_deprecation_date(d).is_none() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!(
                            "model '{model_name}': deprecationDate: '{d}' must be a date 'YYYY-MM-DD' (defaults to 13:00 UTC) or a whole-hour UTC instant 'YYYY-MM-DDTHH:00:00Z' (e.g. 2026-01-01T00:00:00Z); off-hour or non-UTC datetimes are not accepted"
                        ),
                        "invalid_request".to_string(),
                    )),
                ));
            }
        }
        // `openrouter.slug` override must be a lowercase `author/slug` (the
        // canonical shape OpenRouter uses in its `/api/v1/models` ids, e.g.
        // `z-ai/glm-5.1`). Reject anything else at the write path so the
        // catalog can't emit a slug OpenRouter would refuse to match. An
        // explicit `null` (clear) and an omitted field both skip this check.
        if let Some(Some(slug)) = &request.openrouter_slug {
            if !is_valid_openrouter_slug(slug) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!(
                            "model '{model_name}': openrouterSlug: '{slug}' is not a valid OpenRouter slug; expected lowercase 'author/slug' (e.g. 'z-ai/glm-5.1')"
                        ),
                        "invalid_request".to_string(),
                    )),
                ));
            }
        }
    }

    // Extract admin user context for audit tracking
    let admin_user_id = admin_user.0.id;
    let admin_user_email = admin_user.0.email.clone();

    // Convert API request to service request
    // Note: Default owned_by value is applied in the repository layer during INSERT,
    // not here, so we can distinguish between CREATE (apply default) and UPDATE (preserve old)
    let models = batch_request
        .iter()
        .map(|(model_name, request)| {
            (
                model_name.clone(),
                UpdateModelAdminRequest {
                    input_cost_per_token: request.input_cost_per_token.as_ref().map(|p| p.amount),
                    output_cost_per_token: request.output_cost_per_token.as_ref().map(|p| p.amount),
                    cost_per_image: request.cost_per_image.as_ref().map(|p| p.amount),
                    cache_read_cost_per_token: request
                        .cache_read_cost_per_token
                        .as_ref()
                        .map(|p| p.amount),
                    model_display_name: request.model_display_name.clone(),
                    model_description: request.model_description.clone(),
                    model_icon: request.model_icon.clone(),
                    context_length: request.context_length,
                    verifiable: request.verifiable,
                    is_active: request.is_active,
                    allow_free: request.allow_free,
                    aliases: request.aliases.clone(),
                    owned_by: request.owned_by.clone(),
                    provider_type: request.provider_type.clone(),
                    provider_config: request.provider_config.clone(),
                    attestation_supported: request.attestation_supported,
                    input_modalities: request.input_modalities.clone(),
                    output_modalities: request.output_modalities.clone(),
                    inference_url: request.inference_url.clone(),
                    hugging_face_id: request.hugging_face_id.clone(),
                    quantization: request.quantization.clone(),
                    max_output_length: request.max_output_length,
                    supported_sampling_parameters: request.supported_sampling_parameters.clone(),
                    supported_features: request.supported_features.clone(),
                    datacenters: crate::models::Datacenter::to_codes(request.datacenters.clone()),
                    // Tri-state passes straight through: outer None = leave
                    // unchanged, Some(None) = clear, Some(Some(v)) = set.
                    is_ready: request.is_ready,
                    // Tri-state. Outer None = leave unchanged, Some(None) =
                    // clear. For Some(Some(s)) the string was already validated
                    // above, so parse+normalize it to a stored timestamp.
                    deprecation_date: request
                        .deprecation_date
                        .as_ref()
                        .map(|inner| inner.as_deref().and_then(parse_deprecation_date)),
                    // Tri-state passes straight through: outer None = leave
                    // unchanged, Some(None) = clear, Some(Some(v)) = set. The
                    // value was already shape-validated above.
                    openrouter_slug: request.openrouter_slug.clone(),
                    change_reason: request.change_reason.clone(),
                    changed_by_user_id: Some(admin_user_id),
                    changed_by_user_email: Some(admin_user_email.clone()),
                },
            )
        })
        .collect();

    let updated_models = app_state
        .admin_service
        .batch_upsert_models(models)
        .await
        .map_err(|e| {
            error!("Failed to upsert models");
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
                        format!("Failed to upsert models, error: {e:?}"),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    // Update providers at runtime so changes take effect without server restart.
    // Unregister first, then re-register — this handles type transitions
    // (e.g., inference_url → external) and deactivations cleanly.

    // Unregister models that are deactivated or changing provider type.
    // This covers: is_active=false, provider_type changed, inference_url cleared.
    // Re-registration below will add back the ones that should still be active.
    for (model_name, request) in &batch_request {
        // Never tear down a pinned (out-of-band, config-managed) provider such as
        // Chutes. The re-registration below only covers DB-discovered providers
        // (inference-url / external), so unregistering a pinned provider here —
        // e.g. on a PATCH carrying `provider_type: "chutes"` or activating the
        // model — would leave an active catalog row with no serving provider until
        // restart. Pricing/metadata still applied via batch_upsert_models above;
        // serving is gated by the catalog `is_active`, not by this in-memory entry.
        //
        // NOTE: with tiered fallback this now also covers a NEAR-served canonical id
        // that has a Chutes fallback (it's `is_pinned`). So a PATCH that changes its
        // `inference_url` skips the eager `unregister_provider` here, but the NEW url
        // is still re-registered below and `load_inference_url_models`' atomic update
        // drops the *replaced* NEAR provider from `model_to_providers`, prunes its
        // `pubkey_to_providers` entries, and prunes its per-provider failure counter
        // (the prune is filtered against still-live pointers, so the coexisting
        // pinned Chutes fallback keeps its counter) — no stale routing or counter
        // state for the replaced provider is left behind. Behavior stays safe (pubkey
        // intersection + catalog `is_active` gating).
        if app_state.inference_provider_pool.is_pinned(model_name) {
            continue;
        }
        let is_inactive = request.is_active == Some(false);
        let has_type_change = request.provider_type.is_some() || request.inference_url.is_some();
        if is_inactive || has_type_change {
            app_state
                .inference_provider_pool
                .unregister_provider(model_name)
                .await;
        }
    }

    // Register inference_url models (our own vLLM/SGLang backends)
    // Only for active, non-external models with an inference_url set
    let inference_url_models: Vec<(String, String)> = batch_request
        .iter()
        .filter_map(|(model_name, request)| {
            let is_active = request.is_active != Some(false);
            let is_external = request.provider_type.as_deref() == Some("external");
            if is_active && !is_external {
                request
                    .inference_url
                    .clone()
                    .map(|url| (model_name.clone(), url))
            } else {
                None
            }
        })
        .collect();

    if !inference_url_models.is_empty() {
        tracing::info!(
            count = inference_url_models.len(),
            "Registering inference_url models at runtime"
        );
        app_state
            .inference_provider_pool
            .load_inference_url_models(inference_url_models)
            .await;
    }

    // Register external providers (OpenAI, Anthropic, Gemini, etc.)
    let external_models: Vec<(String, serde_json::Value)> = batch_request
        .iter()
        .filter_map(|(model_name, request)| {
            let is_external = request.provider_type.as_deref() == Some("external");
            let is_active = request.is_active != Some(false);

            if is_external && is_active {
                request
                    .provider_config
                    .clone()
                    .map(|config| (model_name.clone(), config))
            } else {
                None
            }
        })
        .collect();

    if !external_models.is_empty() {
        tracing::info!(
            count = external_models.len(),
            "Registering external providers at runtime"
        );
        if let Err(e) = app_state
            .inference_provider_pool
            .load_external_providers(external_models)
            .await
        {
            tracing::warn!(error = %e, "Failed to register some external providers at runtime");
        }
    }

    // Fire GitHub repository_dispatch for each model this PATCH (re)loaded.
    // Downstream automation (validate / promote workflows in
    // cvm-ansible-playbooks) listens for the configured event_type and reacts.
    // Fire-and-forget: a GitHub outage does not block the PATCH. Only enabled
    // on staging cloud-api via ENABLE_GITHUB_DISPATCH; production keeps it off
    // so promote-driven prod PATCHes do not recursively re-fire the chain.
    //
    // Skip deactivations: a PATCH that sets is_active=false is an unload, not a
    // load, and must not trigger validate/promote of a model being taken
    // offline. Dispatch the whole set from a single background task so a large
    // batch does not burst N concurrent requests at GitHub's dispatch API
    // (which would trip secondary rate limits and silently drop events).
    let dispatch_model_ids: Vec<String> = batch_request
        .iter()
        .filter(|(_, request)| request.is_active != Some(false))
        .map(|(model_id, _)| model_id.clone())
        .collect();
    if !dispatch_model_ids.is_empty() {
        let dispatcher = app_state.github_dispatcher.clone();
        // Carry the current tracing span into the fire-and-forget task;
        // tokio::spawn does not inherit it, so dispatch-failure warnings would
        // otherwise lose the request's log context.
        tokio::spawn(
            async move {
                for model_id in dispatch_model_ids {
                    if let Err(e) = dispatcher.dispatch_model_loaded(&model_id).await {
                        tracing::warn!(
                            error = %e,
                            model_id = %model_id,
                            "GitHub dispatch failed; manual workflow trigger may be required"
                        );
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );
    }

    // Convert to API response - map from HashMap to Vec
    // The key in the HashMap is the canonical model_name
    let api_models: Vec<ModelWithPricing> = updated_models
        .into_iter()
        .map(|(model_name, updated_model)| ModelWithPricing {
            model_id: model_name,
            input_cost_per_token: DecimalPrice {
                amount: updated_model.input_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            output_cost_per_token: DecimalPrice {
                amount: updated_model.output_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            cost_per_image: DecimalPrice {
                amount: updated_model.cost_per_image,
                scale: 9,
                currency: "USD".to_string(),
            },
            cache_read_cost_per_token: DecimalPrice {
                amount: updated_model.cache_read_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            metadata: ModelMetadata {
                verifiable: updated_model.verifiable,
                context_length: updated_model.context_length,
                model_display_name: updated_model.model_display_name,
                model_description: updated_model.model_description,
                model_icon: updated_model.model_icon,
                owned_by: updated_model.owned_by,
                aliases: updated_model.aliases,
                provider_type: updated_model.provider_type,
                provider_config: crate::routes::common::redact_provider_config(
                    updated_model.provider_config,
                ),
                attestation_supported: updated_model.attestation_supported,
                architecture: ModelArchitecture::from_options(
                    updated_model.input_modalities,
                    updated_model.output_modalities,
                ),
                inference_url: updated_model.inference_url,
                hugging_face_id: updated_model.hugging_face_id,
                quantization: updated_model.quantization,
                max_output_length: updated_model.max_output_length,
                supported_sampling_parameters: updated_model.supported_sampling_parameters,
                supported_features: updated_model.supported_features,
                datacenters: crate::models::Datacenter::from_codes(updated_model.datacenters),
                is_ready: updated_model.is_ready,
                deprecation_date: updated_model
                    .deprecation_date
                    .as_ref()
                    .map(format_deprecation_date),
                openrouter_slug: updated_model.openrouter_slug,
            },
        })
        .collect();

    Ok(ResponseJson(api_models))
}

/// List all models (Admin only)
///
/// Returns a paginated list of all models in the system. By default, only active models are returned.
/// Use `include_inactive=true` to also include disabled models.
#[utoipa::path(
    get,
    path = "/v1/admin/models",
    tag = "Admin",
    params(
        ("limit" = Option<i64>, Query, description = "Maximum number of models to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Number of models to skip (default: 0)"),
        ("include_inactive" = Option<bool>, Query, description = "Whether to include inactive (disabled) models (default: false)")
    ),
    responses(
        (status = 200, description = "Models retrieved successfully", body = AdminModelListResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_models(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    axum::extract::Query(params): axum::extract::Query<ListModelsQueryParams>,
) -> Result<ResponseJson<AdminModelListResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "List models request with limit={}, offset={}, include_inactive={}",
        params.limit, params.offset, params.include_inactive
    );

    let (models, total) = app_state
        .admin_service
        .list_models(params.include_inactive, params.limit, params.offset)
        .await
        .map_err(|e| {
            error!("Failed to list models");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to retrieve models: {e}"),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    let api_models: Vec<AdminModelWithPricing> = models
        .into_iter()
        .map(|model| AdminModelWithPricing {
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
                aliases: model.aliases,
                owned_by: model.owned_by,
                provider_type: model.provider_type,
                provider_config: crate::routes::common::redact_provider_config(
                    model.provider_config,
                ),
                attestation_supported: model.attestation_supported,
                architecture: ModelArchitecture::from_options(
                    model.input_modalities,
                    model.output_modalities,
                ),
                inference_url: model.inference_url,
                hugging_face_id: model.hugging_face_id,
                quantization: model.quantization,
                max_output_length: model.max_output_length,
                supported_sampling_parameters: model.supported_sampling_parameters,
                supported_features: model.supported_features,
                datacenters: crate::models::Datacenter::from_codes(model.datacenters),
                is_ready: model.is_ready,
                deprecation_date: model.deprecation_date.as_ref().map(format_deprecation_date),
                openrouter_slug: model.openrouter_slug,
            },
            is_active: model.is_active,
            created_at: model.created_at,
            updated_at: model.updated_at,
        })
        .collect();

    let response = AdminModelListResponse {
        models: api_models,
        total,
        limit: params.limit,
        offset: params.offset,
    };

    Ok(ResponseJson(response))
}

/// Get complete history for a model (Admin only)
///
/// Returns the complete history for a specific model, showing all changes over time including pricing,
/// context length, display name, and description.
///
/// **Note:** Model names containing forward slashes (e.g., "Qwen/Qwen3-30B-A3B-Instruct-2507") must be URL-encoded.
/// For example, use "Qwen%2FQwen3-30B-A3B-Instruct-2507" in the URL path.
#[utoipa::path(
    get,
    path = "/v1/admin/models/{model_name}/history",
    tag = "Admin",
    params(
        ("model_name" = String, Path, description = "Model name to get complete history for (URL-encode if it contains slashes)"),
        ("limit" = Option<i64>, Query, description = "Maximum number of history entries to return (default: 50)"),
        ("offset" = Option<i64>, Query, description = "Number of history entries to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Model history retrieved successfully", body = ModelHistoryResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_model_history(
    State(app_state): State<AdminAppState>,
    Path(model_name): Path<String>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<ModelHistoryQueryParams>,
) -> Result<ResponseJson<ModelHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "Get model history request for model: {}, limit={}, offset={}",
        model_name, params.limit, params.offset
    );

    let (history, total) = app_state
        .admin_service
        .get_model_history(&model_name, params.limit, params.offset)
        .await
        .map_err(|e| {
            error!("Failed to get model history");
            match e {
                services::admin::AdminError::ModelNotFound(_) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        format!("Model '{model_name}' not found"),
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
                        "Failed to retrieve model history".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let history_entries: Vec<ModelHistoryEntry> = history
        .into_iter()
        .map(|h| ModelHistoryEntry {
            id: h.id.to_string(),
            model_id: h.model_id.to_string(),
            input_cost_per_token: DecimalPrice {
                amount: h.input_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            output_cost_per_token: DecimalPrice {
                amount: h.output_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            cost_per_image: DecimalPrice {
                amount: h.cost_per_image,
                scale: 9,
                currency: "USD".to_string(),
            },
            cache_read_cost_per_token: DecimalPrice {
                amount: h.cache_read_cost_per_token,
                scale: 9,
                currency: "USD".to_string(),
            },
            context_length: h.context_length,
            model_name: h.model_name,
            model_display_name: h.model_display_name,
            model_description: h.model_description,
            model_icon: h.model_icon,
            verifiable: h.verifiable,
            is_active: h.is_active,
            owned_by: h.owned_by,
            effective_from: h.effective_from.to_rfc3339(),
            effective_until: h.effective_until.map(|dt| dt.to_rfc3339()),
            changed_by_user_id: h.changed_by_user_id.map(|id| id.to_string()),
            changed_by_user_email: h.changed_by_user_email,
            change_reason: h.change_reason,
            created_at: h.created_at.to_rfc3339(),
            input_modalities: h.input_modalities,
            output_modalities: h.output_modalities,
            inference_url: h.inference_url,
            hugging_face_id: h.hugging_face_id,
            quantization: h.quantization,
            max_output_length: h.max_output_length,
            supported_sampling_parameters: h.supported_sampling_parameters,
            supported_features: h.supported_features,
            datacenters: crate::models::Datacenter::from_codes(h.datacenters),
            is_ready: h.is_ready,
            deprecation_date: h.deprecation_date.as_ref().map(format_deprecation_date),
            openrouter_slug: h.openrouter_slug,
        })
        .collect();

    let response = ModelHistoryResponse {
        model_name,
        history: history_entries,
        total,
        limit: params.limit,
        offset: params.offset,
    };

    Ok(ResponseJson(response))
}

/// Update organization limits (Admin only)
///
/// Updates spending limits for a specific organization. This endpoint is typically called by
/// a billing service with an admin API key when a customer makes a purchase.
#[utoipa::path(
    patch,
    path = "/v1/admin/organizations/{org_id}/limits",
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
        ("session_token" = [])
    )
)]
pub async fn update_organization_limits(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Extension(admin_user): Extension<AdminUser>, // Require admin auth
    ResponseJson(request): ResponseJson<UpdateOrganizationLimitsRequest>,
) -> Result<ResponseJson<UpdateOrganizationLimitsResponse>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!(
        "Update organization limits request for org_id: {}, type: {}, source: {:?}, amount: {} nano-dollars, currency: {}",
        org_id, request.credit_type, request.source, request.spend_limit.amount, request.spend_limit.currency
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

    // Extract admin user ID and email from authenticated user
    let admin_user_id = admin_user.0.id;
    let admin_user_email = admin_user.0.email.clone();

    // Convert API request to service request
    let service_request = services::admin::OrganizationLimitsUpdate {
        spend_limit: request.spend_limit.amount,
        credit_type: request.credit_type.to_string(),
        source: request.source,
        currency: request.spend_limit.currency.to_uppercase(),
        changed_by: request.changed_by,
        change_reason: request.change_reason,
        changed_by_user_id: Some(admin_user_id),
        changed_by_user_email: Some(admin_user_email),
    };

    // Update organization limits via admin service
    let updated_limits = app_state
        .admin_service
        .update_organization_limits(organization_id, service_request)
        .await
        .map_err(|e| {
            error!("Failed to update organization limits");
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
    let credit_type_enum = match updated_limits.credit_type.to_lowercase().as_str() {
        "grant" => CreditType::Grant,
        "payment" => CreditType::Payment,
        _ => CreditType::Payment, // Default fallback (should not happen)
    };

    let response = UpdateOrganizationLimitsResponse {
        organization_id: updated_limits.organization_id.to_string(),
        credit_type: credit_type_enum,
        source: updated_limits.source,
        spend_limit: SpendLimit {
            amount: updated_limits.spend_limit,
            scale: 9, // Always scale 9 (nano-dollars)
            currency: updated_limits.currency,
        },
        updated_at: updated_limits.effective_from.to_rfc3339(),
    };

    Ok(ResponseJson(response))
}

/// Get limits history for an organization (Admin only)
///
/// Returns the complete limits history for a specific organization, showing all limits changes over time.
/// Get limits history for an organization (Admin only)
///
/// Returns the complete limits history for a specific organization, showing all limits changes over time.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/limits/history",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "The organization's ID (as a UUID)"),
        ("limit" = Option<i64>, Query, description = "Maximum number of history records to return (default: 50)"),
        ("offset" = Option<i64>, Query, description = "Number of records to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Limits history retrieved successfully", body = OrgLimitsHistoryResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_limits_history(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<OrgLimitsHistoryQueryParams>,
) -> Result<ResponseJson<OrgLimitsHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    let organization_uuid = match uuid::Uuid::parse_str(&org_id) {
        Ok(id) => id,
        Err(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Invalid organization ID format".to_string(),
                    "invalid_request".to_string(),
                )),
            ));
        }
    };

    debug!(
        "Get limits history for organization_id={}, limit={}, offset={}",
        org_id, params.limit, params.offset
    );

    let (history, total) = app_state
        .admin_service
        .get_organization_limits_history(organization_uuid, params.limit, params.offset)
        .await
        .map_err(|e| {
            error!("Failed to retrieve organization limits history");
            match e {
                services::admin::AdminError::OrganizationNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        msg,
                        "organization_not_found".to_string(),
                    )),
                ),
                services::admin::AdminError::Unauthorized(msg) => (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve limits history".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let entries: Vec<OrgLimitsHistoryEntry> = history
        .into_iter()
        .map(|h| {
            let credit_type_enum = match h.credit_type.to_lowercase().as_str() {
                "grant" => CreditType::Grant,
                "payment" => CreditType::Payment,
                _ => CreditType::Payment,
            };
            OrgLimitsHistoryEntry {
                id: h.id.to_string(),
                organization_id: h.organization_id.to_string(),
                credit_type: credit_type_enum,
                source: h.source,
                spend_limit: SpendLimit {
                    amount: h.spend_limit,
                    scale: 9,
                    currency: h.currency,
                },
                effective_from: h.effective_from.to_rfc3339(),
                effective_until: h.effective_until.map(|dt| dt.to_rfc3339()),
                changed_by: h.changed_by,
                change_reason: h.change_reason,
                changed_by_user_id: h.changed_by_user_id.map(|id| id.to_string()),
                changed_by_user_email: h.changed_by_user_email,
                created_at: h.created_at.to_rfc3339(),
            }
        })
        .collect();

    let response = OrgLimitsHistoryResponse {
        history: entries,
        total,
        limit: params.limit,
        offset: params.offset,
    };

    Ok(ResponseJson(response))
}

/// Get organization balance (Admin only)
///
/// Returns the current spending balance for an organization without requiring
/// the caller to be a member of that organization. Intended for trusted
/// automated billing services.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/usage/balance",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization balance", body = OrganizationBalanceResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_admin_organization_balance(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<ResponseJson<OrganizationBalanceResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Admin get organization balance request for org_id: {}",
        org_id
    );

    let organization_id = uuid::Uuid::parse_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID format".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    compute_organization_balance_response(&*app_state.usage_service, organization_id)
        .await
        .map(ResponseJson)
}

/// Delete a model (Admin only)
///
/// Soft deletes a model by setting is_active to false. This preserves historical usage records
/// that reference the model name while preventing it from being used in new requests.
///
/// **Note:** Model names containing forward slashes (e.g., "Qwen/Qwen3-30B-A3B-Instruct-2507") must be URL-encoded.
/// For example, use "Qwen%2FQwen3-30B-A3B-Instruct-2507" in the URL path.
#[utoipa::path(
    delete,
    path = "/v1/admin/models/{model_name}",
    tag = "Admin",
    params(
        ("model_name" = String, Path, description = "Model name to delete (URL-encode if it contains slashes)")
    ),
    request_body = DeleteModelRequest,
    responses(
        (status = 204, description = "Model deleted successfully"),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn delete_model(
    State(app_state): State<AdminAppState>,
    Path(model_name): Path<String>,
    Extension(admin_user): Extension<AdminUser>,
    request: Option<Json<DeleteModelRequest>>,
) -> Result<StatusCode, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Delete model request for: {}", model_name);

    // Extract admin user context for audit tracking
    let admin_user_id = admin_user.0.id;
    let admin_user_email = admin_user.0.email.clone();
    let change_reason = request.and_then(|Json(req)| req.change_reason);

    app_state
        .admin_service
        .delete_model(
            &model_name,
            change_reason,
            Some(admin_user_id),
            Some(admin_user_email),
        )
        .await
        .map_err(|e| {
            error!("Failed to delete model");
            match e {
                services::admin::AdminError::ModelNotFound(_) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        format!("Model '{model_name}' not found"),
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
                        "Failed to delete model".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    // Unregister external provider if it was registered.
    // No-op for vLLM models (discovered, not registered). Skip pinned models
    // (e.g. a NEAR-served canonical id with a Chutes fallback): unregistering would
    // tear down the config-pinned provider too, recoverable only by restart. The
    // catalog delete already 404s the model via resolve_and_get_model's is_active
    // gate; config remains the source of truth for the pinned provider.
    if app_state.inference_provider_pool.is_pinned(&model_name) {
        tracing::warn!(
            model = %model_name,
            "delete_model: skipping unregister of a pinned (config-managed) provider; \
             the catalog row is removed (model 404s), pinned provider left intact"
        );
    } else {
        app_state
            .inference_provider_pool
            .unregister_provider(&model_name)
            .await;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Deprecate a model in favor of another (Admin only)
///
/// Atomically marks `modelId` as deprecated and routes its traffic to
/// `successorModelId`:
/// 1. Adds `modelId` as an alias of `successorModelId`, so existing clients
///    sending `model: "<modelId>"` keep working — the alias resolver rewrites
///    the request-side `model` field to the successor before backend dispatch,
///    and the response's `model` field reflects the canonical (successor) name.
/// 2. Re-points any pre-existing inbound aliases of `modelId` at the
///    successor, so historical aliases keep resolving.
/// 3. Sets `modelId.isActive = false` so it is hidden from public
///    `GET /v1/models` and from `GET /v1/admin/models` unless
///    `include_inactive=true`.
/// 4. Records a `model_history` entry for audit purposes.
///
/// All steps run in a single DB transaction. If the successor is inactive or
/// either model is missing, returns 404 without modifying state.
#[utoipa::path(
    post,
    path = "/v1/admin/models/deprecate",
    tag = "Admin",
    request_body = DeprecateModelRequest,
    responses(
        (status = 200, description = "Model deprecated successfully", body = DeprecateModelResponse),
        (status = 400, description = "Invalid request (e.g. self-deprecation, empty model id)", body = ErrorResponse),
        (status = 404, description = "Either model not found, or successor is not active", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn deprecate_model(
    State(app_state): State<AdminAppState>,
    Extension(admin_user): Extension<AdminUser>,
    ResponseJson(req): ResponseJson<DeprecateModelRequest>,
) -> Result<ResponseJson<DeprecateModelResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    // Trim once at the boundary so the service, repo, log lines, provider
    // unregister, and the response all see the same canonical names.
    let model_id = req.model_id.trim().to_string();
    let successor_model_id = req.successor_model_id.trim().to_string();

    debug!(
        "Deprecate model: model_id={}, successor_model_id={}",
        model_id, successor_model_id
    );

    let admin_user_id = admin_user.0.id;
    let admin_user_email = admin_user.0.email.clone();

    let outcome = app_state
        .admin_service
        .deprecate_model(
            &model_id,
            &successor_model_id,
            req.change_reason.clone(),
            Some(admin_user_id),
            Some(admin_user_email),
        )
        .await
        .map_err(|e| {
            // Discriminant only — never log error message contents at this
            // layer (could surface validation strings about user input).
            error!(
                error_kind = ?std::mem::discriminant(&e),
                model_id = %model_id,
                successor_model_id = %successor_model_id,
                "Failed to deprecate model"
            );
            admin_error_to_response(e)
        })?;

    // Stop sending live traffic to the deprecated provider. The alias
    // resolver will route requests with the deprecated model_id to the
    // successor on subsequent calls — so this prevents in-flight resolution
    // from picking up a stale provider reference for the now-inactive model.
    // Skip pinned models (e.g. a NEAR-served canonical id with a Chutes fallback):
    // unregistering would tear down the config-pinned provider too (recoverable
    // only by restart), and the alias already routes the deprecated id to the
    // successor, so the pinned provider isn't reached.
    if app_state.inference_provider_pool.is_pinned(&model_id) {
        tracing::warn!(
            model = %model_id,
            "deprecate_model: skipping unregister of a pinned (config-managed) provider; \
             the alias routes the deprecated id to its successor, pinned provider left intact"
        );
    } else {
        app_state
            .inference_provider_pool
            .unregister_provider(&model_id)
            .await;
    }

    let to_api = |model_name: String, m: services::admin::ModelPricing| ModelWithPricing {
        model_id: model_name,
        input_cost_per_token: DecimalPrice {
            amount: m.input_cost_per_token,
            scale: 9,
            currency: "USD".to_string(),
        },
        output_cost_per_token: DecimalPrice {
            amount: m.output_cost_per_token,
            scale: 9,
            currency: "USD".to_string(),
        },
        cost_per_image: DecimalPrice {
            amount: m.cost_per_image,
            scale: 9,
            currency: "USD".to_string(),
        },
        cache_read_cost_per_token: DecimalPrice {
            amount: m.cache_read_cost_per_token,
            scale: 9,
            currency: "USD".to_string(),
        },
        metadata: ModelMetadata {
            verifiable: m.verifiable,
            context_length: m.context_length,
            model_display_name: m.model_display_name,
            model_description: m.model_description,
            model_icon: m.model_icon,
            owned_by: m.owned_by,
            aliases: m.aliases,
            provider_type: m.provider_type,
            provider_config: crate::routes::common::redact_provider_config(m.provider_config),
            attestation_supported: m.attestation_supported,
            architecture: ModelArchitecture::from_options(m.input_modalities, m.output_modalities),
            inference_url: m.inference_url,
            hugging_face_id: m.hugging_face_id,
            quantization: m.quantization,
            max_output_length: m.max_output_length,
            supported_sampling_parameters: m.supported_sampling_parameters,
            supported_features: m.supported_features,
            datacenters: crate::models::Datacenter::from_codes(m.datacenters),
            is_ready: m.is_ready,
            deprecation_date: m.deprecation_date.as_ref().map(format_deprecation_date),
            openrouter_slug: m.openrouter_slug,
        },
    };

    Ok(ResponseJson(DeprecateModelResponse {
        deprecated: to_api(model_id, outcome.deprecated),
        successor: to_api(successor_model_id, outcome.successor),
        aliases_carried: outcome.aliases_carried,
    }))
}

/// Preview affected admins for a planned model deprecation (Admin only).
#[utoipa::path(
    post,
    path = "/v1/admin/models/{model_name}/deprecation/preview",
    tag = "Admin",
    request_body = ModelDeprecationRequest,
    responses(
        (status = 200, description = "Deprecation notification preview", body = ModelDeprecationPreviewResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 404, description = "Model or successor not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn preview_model_deprecation(
    State(app_state): State<AdminAppState>,
    Path(model_name): Path<String>,
    Extension(_admin_user): Extension<AdminUser>,
    ResponseJson(req): ResponseJson<ModelDeprecationRequest>,
) -> Result<ResponseJson<ModelDeprecationPreviewResponse>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    let deprecation_date = parse_deprecation_date(&req.deprecation_date).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                format!(
                    "deprecationDate: '{}' must be a date 'YYYY-MM-DD' (defaults to 13:00 UTC) or a whole-hour UTC instant 'YYYY-MM-DDTHH:00:00Z'",
                    req.deprecation_date
                ),
                "invalid_request".to_string(),
            )),
        )
    })?;

    let preview = app_state
        .admin_service
        .preview_model_deprecation(&model_name, &req.successor_model_id, deprecation_date)
        .await
        .map_err(admin_error_to_response)?;

    Ok(ResponseJson(ModelDeprecationPreviewResponse {
        recipient_count: preview.recipient_count,
        organization_count: preview.organization_count,
        usage_window_days: preview.usage_window_days,
    }))
}

/// Confirm a planned model deprecation, update the catalog, and notify affected admins.
#[utoipa::path(
    post,
    path = "/v1/admin/models/{model_name}/deprecation/confirm",
    tag = "Admin",
    request_body = ModelDeprecationRequest,
    responses(
        (status = 200, description = "Deprecation confirmed and notifications attempted", body = ModelDeprecationConfirmResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 404, description = "Model or successor not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn confirm_model_deprecation(
    State(app_state): State<AdminAppState>,
    Path(model_name): Path<String>,
    Extension(admin_user): Extension<AdminUser>,
    ResponseJson(req): ResponseJson<ModelDeprecationRequest>,
) -> Result<ResponseJson<ModelDeprecationConfirmResponse>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    let deprecation_date = parse_deprecation_date(&req.deprecation_date).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                format!(
                    "deprecationDate: '{}' must be a date 'YYYY-MM-DD' (defaults to 13:00 UTC) or a whole-hour UTC instant 'YYYY-MM-DDTHH:00:00Z'",
                    req.deprecation_date
                ),
                "invalid_request".to_string(),
            )),
        )
    })?;

    let result = app_state
        .admin_service
        .confirm_model_deprecation(
            &model_name,
            &req.successor_model_id,
            deprecation_date,
            req.change_reason,
            Some(admin_user.0.id),
            Some(admin_user.0.email),
        )
        .await
        .map_err(admin_error_to_response)?;

    Ok(ResponseJson(ModelDeprecationConfirmResponse {
        model_id: result.model_id,
        successor_model_id: result.successor_model_id,
        deprecation_date: format_deprecation_date(&result.deprecation_date),
        recipient_count: result.recipient_count,
        organization_count: result.organization_count,
        sent_count: result.sent_count,
        failed_count: result.failed_count,
        skipped_count: result.skipped_count,
    }))
}

fn usd_price(amount: i64) -> DecimalPrice {
    DecimalPrice {
        amount,
        scale: 9,
        currency: "USD".to_string(),
    }
}

/// Validate the request-level shape of a pricing change batch and convert it
/// to service inputs. Pricing/lead-time/model validation happens in the
/// service layer.
fn pricing_change_inputs_from_request(
    req: &PricingChangeBatchRequest,
) -> Result<Vec<services::admin::PricingChangeInput>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let invalid = |msg: String| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(msg, "invalid_request".to_string())),
        )
    };

    req.changes
        .iter()
        .map(|item| {
            let effective_at = parse_deprecation_date(&item.effective_at).ok_or_else(|| {
                invalid(format!(
                    "model '{}': effectiveAt: '{}' must be a date 'YYYY-MM-DD' (defaults to 13:00 UTC) or a whole-hour UTC instant 'YYYY-MM-DDTHH:00:00Z'",
                    item.model_id, item.effective_at
                ))
            })?;
            for price in [
                &item.input_cost_per_token,
                &item.output_cost_per_token,
                &item.cache_read_cost_per_token,
                &item.cost_per_image,
            ]
            .into_iter()
            .flatten()
            {
                price
                    .validate()
                    .map_err(|e| invalid(format!("model '{}': {e}", item.model_id)))?;
                // Only the amount is stored; responses and notification
                // emails label it USD, so any other currency would silently
                // be billed as USD.
                if !price.currency.eq_ignore_ascii_case("USD") {
                    return Err(invalid(format!(
                        "model '{}': currency must be 'USD'",
                        item.model_id
                    )));
                }
            }
            Ok(services::admin::PricingChangeInput {
                model_name: item.model_id.clone(),
                effective_at,
                new_input_cost_per_token: item.input_cost_per_token.as_ref().map(|p| p.amount),
                new_output_cost_per_token: item.output_cost_per_token.as_ref().map(|p| p.amount),
                new_cache_read_cost_per_token: item
                    .cache_read_cost_per_token
                    .as_ref()
                    .map(|p| p.amount),
                new_cost_per_image: item.cost_per_image.as_ref().map(|p| p.amount),
            })
        })
        .collect()
}

fn scheduled_pricing_change_to_dto(
    change: services::admin::ScheduledPricingChange,
) -> ScheduledPricingChangeDto {
    ScheduledPricingChangeDto {
        id: change.id.to_string(),
        batch_id: change.batch_id.to_string(),
        model_id: change.model_name,
        model_display_name: change.model_display_name,
        status: change.status.as_str().to_string(),
        effective_at: format_deprecation_date(&change.effective_at),
        old_pricing: PricingFields {
            input_cost_per_token: usd_price(change.old_input_cost_per_token),
            output_cost_per_token: usd_price(change.old_output_cost_per_token),
            cache_read_cost_per_token: usd_price(change.old_cache_read_cost_per_token),
            cost_per_image: usd_price(change.old_cost_per_image),
        },
        new_pricing: PricingFieldUpdates {
            input_cost_per_token: change.new_input_cost_per_token.map(usd_price),
            output_cost_per_token: change.new_output_cost_per_token.map(usd_price),
            cache_read_cost_per_token: change.new_cache_read_cost_per_token.map(usd_price),
            cost_per_image: change.new_cost_per_image.map(usd_price),
        },
        applied_at: change.applied_at.map(|dt| dt.to_rfc3339()),
        last_error: change.last_error,
        created_by_user_email: change.created_by_user_email,
        change_reason: change.change_reason,
        created_at: change.created_at.to_rfc3339(),
    }
}

/// Preview a batch of scheduled pricing changes without mutating state (Admin only).
#[utoipa::path(
    post,
    path = "/v1/admin/models/pricing-changes/preview",
    tag = "Admin",
    request_body = PricingChangeBatchRequest,
    responses(
        (status = 200, description = "Pricing change notification preview", body = PricingChangePreviewResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 404, description = "Model not found or inactive", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn preview_model_pricing_changes(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    ResponseJson(req): ResponseJson<PricingChangeBatchRequest>,
) -> Result<ResponseJson<PricingChangePreviewResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let changes = pricing_change_inputs_from_request(&req)?;

    let preview = app_state
        .admin_service
        .preview_pricing_changes(changes)
        .await
        .map_err(admin_error_to_response)?;

    Ok(ResponseJson(PricingChangePreviewResponse {
        recipient_count: preview.recipient_count,
        organization_count: preview.organization_count,
        usage_window_days: preview.usage_window_days,
        models: preview
            .models
            .into_iter()
            .map(|m| PricingChangeModelPreviewDto {
                model_id: m.model_name,
                model_display_name: m.model_display_name,
                effective_at: format_deprecation_date(&m.effective_at),
                recipient_count: m.recipient_count,
                organization_count: m.organization_count,
                old_pricing: PricingFields {
                    input_cost_per_token: usd_price(m.old_input_cost_per_token),
                    output_cost_per_token: usd_price(m.old_output_cost_per_token),
                    cache_read_cost_per_token: usd_price(m.old_cache_read_cost_per_token),
                    cost_per_image: usd_price(m.old_cost_per_image),
                },
                new_pricing: PricingFieldUpdates {
                    input_cost_per_token: m.new_input_cost_per_token.map(usd_price),
                    output_cost_per_token: m.new_output_cost_per_token.map(usd_price),
                    cache_read_cost_per_token: m.new_cache_read_cost_per_token.map(usd_price),
                    cost_per_image: m.new_cost_per_image.map(usd_price),
                },
            })
            .collect(),
    }))
}

/// Confirm a batch of scheduled pricing changes and notify affected admins (Admin only).
///
/// Persists the schedule (the background scheduler applies each change at its
/// effective date) and sends one consolidated email per affected recipient.
#[utoipa::path(
    post,
    path = "/v1/admin/models/pricing-changes/confirm",
    tag = "Admin",
    request_body = PricingChangeBatchRequest,
    responses(
        (status = 200, description = "Pricing changes scheduled and notifications attempted", body = PricingChangeConfirmResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 404, description = "Model not found or inactive", body = ErrorResponse),
        (status = 409, description = "A pending pricing change already exists for a model", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn confirm_model_pricing_changes(
    State(app_state): State<AdminAppState>,
    Extension(admin_user): Extension<AdminUser>,
    ResponseJson(req): ResponseJson<PricingChangeBatchRequest>,
) -> Result<ResponseJson<PricingChangeConfirmResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let changes = pricing_change_inputs_from_request(&req)?;
    let batch_id = req.batch_id.unwrap_or_else(Uuid::new_v4);

    let result = app_state
        .admin_service
        .confirm_pricing_changes(
            batch_id,
            changes,
            req.change_reason,
            Some(admin_user.0.id),
            Some(admin_user.0.email),
        )
        .await
        .map_err(admin_error_to_response)?;

    Ok(ResponseJson(PricingChangeConfirmResponse {
        batch_id: result.batch_id.to_string(),
        recipient_count: result.recipient_count,
        organization_count: result.organization_count,
        sent_count: result.sent_count,
        failed_count: result.failed_count,
        skipped_count: result.skipped_count,
        changes: result
            .changes
            .into_iter()
            .map(scheduled_pricing_change_to_dto)
            .collect(),
    }))
}

#[derive(Debug, serde::Deserialize)]
pub struct ListPricingChangesQueryParams {
    /// Filter by status (pending, applying, applied, cancelled, failed).
    pub status: Option<String>,
    #[serde(default = "default_pricing_changes_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_pricing_changes_limit() -> i64 {
    100
}

/// List scheduled pricing changes (Admin only).
#[utoipa::path(
    get,
    path = "/v1/admin/models/pricing-changes",
    tag = "Admin",
    params(
        ("status" = Option<String>, Query, description = "Filter by status: pending, applying, applied, cancelled, failed. Omit for all."),
        ("limit" = Option<i64>, Query, description = "Maximum number of changes to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Number of changes to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Scheduled pricing changes", body = ListPricingChangesResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_model_pricing_changes(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    Query(params): Query<ListPricingChangesQueryParams>,
) -> Result<ResponseJson<ListPricingChangesResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let status = params
        .status
        .as_deref()
        .map(|s| {
            services::admin::ScheduledPricingChangeStatus::parse(s).ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!(
                            "status: '{s}' must be one of pending, applying, applied, cancelled, failed"
                        ),
                        "invalid_request".to_string(),
                    )),
                )
            })
        })
        .transpose()?;

    let (changes, total) = app_state
        .admin_service
        .list_pricing_changes(status, params.limit, params.offset)
        .await
        .map_err(admin_error_to_response)?;

    Ok(ResponseJson(ListPricingChangesResponse {
        changes: changes
            .into_iter()
            .map(scheduled_pricing_change_to_dto)
            .collect(),
        total,
    }))
}

/// Cancel a pending scheduled pricing change (Admin only).
#[utoipa::path(
    delete,
    path = "/v1/admin/models/pricing-changes/{id}",
    tag = "Admin",
    params(
        ("id" = uuid::Uuid, Path, description = "Scheduled pricing change ID")
    ),
    responses(
        (status = 200, description = "Pricing change cancelled", body = ScheduledPricingChangeDto),
        (status = 404, description = "Pricing change not found or no longer pending", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn cancel_model_pricing_change(
    State(app_state): State<AdminAppState>,
    Path(id): Path<Uuid>,
    Extension(admin_user): Extension<AdminUser>,
) -> Result<ResponseJson<ScheduledPricingChangeDto>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let cancelled = app_state
        .admin_service
        .cancel_pricing_change(id, Some(admin_user.0.id), Some(admin_user.0.email))
        .await
        .map_err(admin_error_to_response)?;

    Ok(ResponseJson(scheduled_pricing_change_to_dto(cancelled)))
}

fn admin_error_to_response(
    e: services::admin::AdminError,
) -> (StatusCode, ResponseJson<ErrorResponse>) {
    match e {
        services::admin::AdminError::InvalidDeprecation(msg)
        | services::admin::AdminError::InvalidPricing(msg)
        | services::admin::AdminError::InvalidLimits(msg) => (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(msg, "invalid_request".to_string())),
        ),
        services::admin::AdminError::ModelNotFound(msg)
        | services::admin::AdminError::ServiceNotFound(msg)
        | services::admin::AdminError::OrganizationNotFound(msg)
        | services::admin::AdminError::PricingChangeNotFound(msg) => (
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(msg, "not_found".to_string())),
        ),
        services::admin::AdminError::PricingChangeConflict(msg) => (
            StatusCode::CONFLICT,
            ResponseJson(ErrorResponse::new(msg, "conflict".to_string())),
        ),
        services::admin::AdminError::Unauthorized(msg) => (
            StatusCode::UNAUTHORIZED,
            ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
        ),
        services::admin::AdminError::InternalError(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                format!("Admin operation failed: {msg}"),
                "internal_server_error".to_string(),
            )),
        ),
    }
}

/// List all registered users with pagination (Admin only)
///
/// Returns a paginated list of all users in the system. Only authenticated admins can perform this operation.
#[utoipa::path(
    get,
    path = "/v1/admin/users",
    tag = "Admin",
    params(
        ("limit" = Option<i64>, Query, description = "Maximum number of users to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Number of users to skip (default: 0)"),
        ("include_organizations" = Option<bool>, Query, description = "Whether to include organization information and spend limits for the first organization owned by each user (default: false)"),
        ("search" = Option<String>, Query, description = "Filter users by email, username, display name, user id, auth provider, or provider user id (case-insensitive partial match)."),
        ("is_active" = Option<bool>, Query, description = "Filter users by active status. Omit to include active and inactive users."),
        ("search_by_name" = Option<String>, Query, description = "Filter users by organization name (case-insensitive match). Only effective when include_organizations=true; separate from user search.")
    ),
    responses(
        (status = 200, description = "Users retrieved successfully", body = ListUsersResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_users(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<ListUsersQueryParams>,
) -> Result<ResponseJson<ListUsersResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "List users request with limit={}, offset={}, include_organizations={}, has_search={}, is_active={:?}, has_search_by_name={}",
        params.limit,
        params.offset,
        params.include_organizations,
        params
            .search
            .as_ref()
            .map(|search| !search.is_empty())
            .unwrap_or(false),
        params.is_active,
        params
            .search_by_name
            .as_ref()
            .map(|search| !search.is_empty())
            .unwrap_or(false)
    );

    let (user_responses, total) = if params.include_organizations {
        // Fetch users with their default organization and spend limit
        let (users_with_orgs, total) = app_state
            .admin_service
            .list_users_with_organizations(
                params.limit,
                params.offset,
                params.search.clone(),
                params.is_active,
                params.search_by_name.clone(),
            )
            .await
            .map_err(|e| {
                error!("Failed to list users with organizations");
                match e {
                    services::admin::AdminError::Unauthorized(msg) => (
                        StatusCode::UNAUTHORIZED,
                        ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                    ),
                    _ => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to retrieve users".to_string(),
                            "internal_server_error".to_string(),
                        )),
                    ),
                }
            })?;

        let responses: Vec<AdminUserResponse> = users_with_orgs
            .into_iter()
            .map(|(u, org_data)| {
                let organizations = org_data.map(|org_info| {
                    let current_usage = org_info.total_spent.map(|total_spent| OrganizationUsage {
                        total_spent,
                        total_spent_display: format_amount(total_spent),
                        total_requests: org_info.total_requests.unwrap_or(0),
                        total_tokens: org_info.total_tokens.unwrap_or(0),
                    });

                    vec![AdminUserOrganizationDetails {
                        id: org_info.id.to_string(),
                        name: org_info.name,
                        description: org_info.description,
                        spend_limit: org_info.spend_limit.map(|amount| SpendLimit {
                            amount,
                            scale: 9,
                            currency: "USD".to_string(),
                        }),
                        current_usage,
                    }]
                });

                AdminUserResponse {
                    id: u.id.to_string(),
                    email: u.email,
                    username: Some(u.username),
                    display_name: u.display_name,
                    avatar_url: u.avatar_url,
                    created_at: u.created_at,
                    last_login_at: u.last_login_at,
                    is_active: u.is_active,
                    auth_provider: u.auth_provider,
                    provider_user_id: u.provider_user_id,
                    organizations,
                }
            })
            .collect();

        (responses, total)
    } else {
        // Return users data only
        let (users, total) = app_state
            .admin_service
            .list_users(
                params.limit,
                params.offset,
                params.search.clone(),
                params.is_active,
            )
            .await
            .map_err(|e| {
                error!("Failed to list users");
                match e {
                    services::admin::AdminError::Unauthorized(msg) => (
                        StatusCode::UNAUTHORIZED,
                        ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                    ),
                    _ => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to retrieve users".to_string(),
                            "internal_server_error".to_string(),
                        )),
                    ),
                }
            })?;

        let responses: Vec<AdminUserResponse> = users
            .into_iter()
            .map(|u| AdminUserResponse {
                id: u.id.to_string(),
                email: u.email,
                username: Some(u.username),
                display_name: u.display_name,
                avatar_url: u.avatar_url,
                created_at: u.created_at,
                last_login_at: u.last_login_at,
                is_active: u.is_active,
                auth_provider: u.auth_provider,
                provider_user_id: u.provider_user_id,
                organizations: None,
            })
            .collect();

        (responses, total)
    };

    let response = ListUsersResponse {
        users: user_responses,
        total,
        limit: params.limit,
        offset: params.offset,
    };

    Ok(ResponseJson(response))
}

/// List all organizations with pagination (Admin only)
///
/// Returns a paginated list of all organizations in the system with their spend limits and usage.
/// Only authenticated admins can perform this operation.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations",
    tag = "Admin",
    params(
        ("limit" = Option<i64>, Query, description = "Maximum number of organizations to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Number of organizations to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Organizations retrieved successfully", body = ListOrganizationsAdminResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_organizations(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<ListOrganizationsQueryParams>,
) -> Result<ResponseJson<ListOrganizationsAdminResponse>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "List organizations request with limit={}, offset={}",
        params.limit, params.offset
    );

    let (organizations, total) = app_state
        .admin_service
        .list_organizations(params.limit, params.offset)
        .await
        .map_err(|e| {
            error!("Failed to list organizations: {:?}", e);
            match e {
                services::admin::AdminError::Unauthorized(msg) => (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve organizations".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let org_responses: Vec<AdminOrganizationResponse> = organizations
        .into_iter()
        .map(admin_org_info_to_response)
        .collect();

    let response = ListOrganizationsAdminResponse {
        organizations: org_responses,
        total,
        limit: params.limit,
        offset: params.offset,
    };

    Ok(ResponseJson(response))
}

/// Get a single organization by id (Admin only)
///
/// Returns one organization with its spend limit and usage. Only authenticated
/// admins can perform this operation. Returns 404 if the organization does not
/// exist or is inactive (consistent with the admin organizations list, which
/// hides inactive orgs).
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}",
    tag = "Admin",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization retrieved successfully", body = AdminOrganizationResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    Path(org_id): Path<Uuid>,
) -> Result<ResponseJson<AdminOrganizationResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Get organization request: org_id={}", org_id);

    let org = app_state
        .admin_service
        .get_organization(org_id)
        .await
        .map_err(|e| match e {
            services::admin::AdminError::OrganizationNotFound(_) => {
                debug!("Organization not found: org_id={}", org_id);
                (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        "Organization not found".to_string(),
                        "not_found".to_string(),
                    )),
                )
            }
            services::admin::AdminError::Unauthorized(msg) => {
                warn!("Unauthorized get organization: org_id={}", org_id);
                (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                )
            }
            other => {
                error!("Failed to get organization: {:?}", other);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve organization".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    Ok(ResponseJson(admin_org_info_to_response(org)))
}

/// Map a service-layer `AdminOrganizationInfo` to the API
/// `AdminOrganizationResponse`. Shared by `list_organizations` and
/// `get_organization` so the spend-limit scale and usage block can't drift
/// between the two.
fn admin_org_info_to_response(
    org: services::admin::AdminOrganizationInfo,
) -> AdminOrganizationResponse {
    let current_usage = org.total_spent.map(|total_spent| OrganizationUsage {
        total_spent,
        total_spent_display: format_amount(total_spent),
        total_requests: org.total_requests.unwrap_or(0),
        total_tokens: org.total_tokens.unwrap_or(0),
    });

    AdminOrganizationResponse {
        id: org.id.to_string(),
        name: org.name,
        description: org.description,
        spend_limit: org.spend_limit.map(|amount| SpendLimit {
            amount,
            scale: 9,
            currency: "USD".to_string(),
        }),
        current_usage,
        created_at: org.created_at,
    }
}

/// Map a raw database role string to the API `MemberRole`. Unknown values fall
/// back to `Member` (the least-privileged role) so a malformed row can never
/// be misrepresented as an owner/admin.
fn member_role_from_db_str(role: &str, member_id: Uuid) -> MemberRole {
    match role {
        "owner" => MemberRole::Owner,
        "admin" => MemberRole::Admin,
        "member" => MemberRole::Member,
        _ => {
            // Unreachable while the `organization_members.role` CHECK constraint
            // holds, but if it is ever relaxed we must not silently present a
            // bogus role as a real one. Fall back to the least-privileged role
            // and surface the anomaly by id only (no raw DB value, per the
            // project's logging rules).
            warn!(
                "Unexpected organization member role for member_id={}; defaulting to 'member'",
                member_id
            );
            MemberRole::Member
        }
    }
}

/// List members of a specific organization (Admin only)
///
/// Returns the members of the given organization with full user details
/// (email, last login, active status), consistent with `/v1/admin/users`.
/// Only authenticated admins can perform this operation.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/members",
    tag = "Admin",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("limit" = Option<i64>, Query, description = "Maximum number of members to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Number of members to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Organization members retrieved successfully", body = ListAdminOrganizationMembersResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_organization_members(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>, // Require admin auth
    Path(org_id): Path<Uuid>,
    Query(params): Query<ListOrganizationsQueryParams>,
) -> Result<
    ResponseJson<ListAdminOrganizationMembersResponse>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "List organization members request: org_id={}, limit={}, offset={}",
        org_id, params.limit, params.offset
    );

    let (members, total) = app_state
        .admin_service
        .list_organization_members(org_id, params.limit, params.offset)
        .await
        .map_err(|e| match e {
            services::admin::AdminError::OrganizationNotFound(_) => {
                // Expected for probes of unknown/deactivated org ids — keep it
                // out of error-level logs so it can't be used to flood them.
                debug!("Organization not found for member list: org_id={}", org_id);
                (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        "Organization not found".to_string(),
                        "not_found".to_string(),
                    )),
                )
            }
            services::admin::AdminError::Unauthorized(msg) => {
                warn!("Unauthorized organization member list: org_id={}", org_id);
                (
                    StatusCode::UNAUTHORIZED,
                    ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
                )
            }
            other => {
                error!("Failed to list organization members: {:?}", other);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve organization members".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    let member_responses: Vec<AdminOrganizationMemberResponse> = members
        .into_iter()
        .map(|m| AdminOrganizationMemberResponse {
            id: m.member_id.to_string(),
            organization_id: m.organization_id.to_string(),
            role: member_role_from_db_str(&m.role, m.member_id),
            joined_at: m.joined_at,
            invited_by: m.invited_by.map(|id| id.to_string()),
            user: AdminUserResponse {
                id: m.user.id.to_string(),
                email: m.user.email,
                username: Some(m.user.username),
                display_name: m.user.display_name,
                avatar_url: m.user.avatar_url,
                created_at: m.user.created_at,
                last_login_at: m.user.last_login_at,
                is_active: m.user.is_active,
                auth_provider: m.user.auth_provider,
                provider_user_id: m.user.provider_user_id,
                organizations: None,
            },
        })
        .collect();

    Ok(ResponseJson(ListAdminOrganizationMembersResponse {
        members: member_responses,
        total,
        limit: params.limit,
        offset: params.offset,
    }))
}

/// List organization invitation email deliveries (Admin only)
///
/// Returns delivery metadata for organization invitation emails without exposing invitation tokens.
#[utoipa::path(
    get,
    path = "/v1/admin/invitation-email-deliveries",
    tag = "Admin",
    params(
        ("limit" = Option<i64>, Query, description = "Maximum number of deliveries to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Number of deliveries to skip (default: 0)"),
        ("organization_id" = Option<Uuid>, Query, description = "Filter by organization ID"),
        ("recipient_email" = Option<String>, Query, description = "Case-insensitive recipient email substring filter"),
        ("email_status" = Option<crate::models::InvitationEmailStatus>, Query, description = "Filter by email delivery status"),
        ("invitation_status" = Option<crate::models::InvitationStatus>, Query, description = "Filter by invitation status"),
        ("created_after" = Option<DateTime<Utc>>, Query, description = "Only invitations created at or after this timestamp"),
        ("created_before" = Option<DateTime<Utc>>, Query, description = "Only invitations created at or before this timestamp")
    ),
    responses(
        (status = 200, description = "Invitation email deliveries retrieved successfully", body = ListAdminInvitationEmailDeliveriesResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_invitation_email_deliveries(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    axum::extract::Query(params): axum::extract::Query<ListInvitationEmailDeliveriesQueryParams>,
) -> Result<
    ResponseJson<ListAdminInvitationEmailDeliveriesResponse>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "List invitation email deliveries request with limit={}, offset={}",
        params.limit, params.offset
    );

    let filters = services::organization::InvitationEmailDeliveryFilters {
        organization_id: params
            .organization_id
            .map(services::organization::OrganizationId),
        recipient_email: params.recipient_email,
        email_status: params
            .email_status
            .map(api_invitation_email_status_to_services),
        invitation_status: params
            .invitation_status
            .map(api_invitation_status_to_services),
        created_after: params.created_after,
        created_before: params.created_before,
    };

    let (deliveries, total) = app_state
        .organization_service
        .list_invitation_email_deliveries(filters, params.limit, params.offset)
        .await
        .map_err(|e| match e {
            services::organization::OrganizationError::InvalidParams(msg) => (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(msg, "invalid_request".to_string())),
            ),
            _ => {
                error!("Failed to list invitation email deliveries: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve invitation email deliveries".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    let response = ListAdminInvitationEmailDeliveriesResponse {
        deliveries: deliveries
            .into_iter()
            .map(services_invitation_email_delivery_to_api)
            .collect(),
        total,
        limit: params.limit,
        offset: params.offset,
    };

    Ok(ResponseJson(response))
}

/// Resend a single organization invitation email (Admin only)
#[utoipa::path(
    post,
    path = "/v1/admin/invitation-email-deliveries/{invitation_id}/resend",
    tag = "Admin",
    params(
        ("invitation_id" = Uuid, Path, description = "Invitation ID")
    ),
    responses(
        (status = 200, description = "Invitation email resend attempted", body = AdminInvitationEmailResendResultResponse),
        (status = 400, description = "Invitation is not pending or has expired", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Invitation not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn resend_invitation_email(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    Path(invitation_id): Path<Uuid>,
) -> Result<
    ResponseJson<AdminInvitationEmailResendResultResponse>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    debug!("Resend invitation email request for {}", invitation_id);

    app_state
        .organization_service
        .resend_invitation_email(invitation_id)
        .await
        .map(services_invitation_resend_result_to_api)
        .map(ResponseJson)
        .map_err(|e| match e {
            services::organization::OrganizationError::NotFound => (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    "Invitation not found".to_string(),
                    "not_found".to_string(),
                )),
            ),
            services::organization::OrganizationError::InvalidParams(msg) => (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(msg, "invalid_request".to_string())),
            ),
            _ => {
                error!("Failed to resend invitation email: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to resend invitation email".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })
}

/// Create platform service (Admin only)
#[utoipa::path(
    post,
    path = "/v1/admin/services",
    tag = "Admin",
    request_body = CreateServiceRequest,
    responses(
        (status = 200, description = "Service created", body = AdminServiceResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(("session_token" = []))
)]
pub async fn create_service(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    Json(req): Json<CreateServiceRequest>,
) -> Result<ResponseJson<AdminServiceResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let s = app_state
        .admin_service
        .create_service(
            &req.service_name,
            &req.display_name,
            req.description.as_deref(),
            req.unit,
            req.cost_per_unit,
        )
        .await
        .map_err(|e| {
            error!("Failed to create service: {:?}", e);
            match e {
                services::admin::AdminError::InvalidPricing(msg) => (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(msg, "invalid_request".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to create service".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;
    Ok(ResponseJson(s.into()))
}

/// Update platform service (Admin only; display_name, description, cost_per_unit, is_active)
#[utoipa::path(
    patch,
    path = "/v1/admin/services/{id}",
    tag = "Admin",
    params(("id" = uuid::Uuid, Path, description = "Service ID")),
    request_body = UpdateServiceRequest,
    responses(
        (status = 200, description = "Service updated", body = AdminServiceResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(("session_token" = []))
)]
pub async fn update_service(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    Path(id): Path<uuid::Uuid>,
    Json(req): Json<UpdateServiceRequest>,
) -> Result<ResponseJson<AdminServiceResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let s = app_state
        .admin_service
        .update_service(
            id,
            req.display_name.as_deref(),
            req.description.as_deref(),
            req.cost_per_unit,
            req.is_active,
        )
        .await
        .map_err(|e| {
            error!("Failed to update service: {:?}", e);
            match e {
                services::admin::AdminError::InvalidPricing(msg) => (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(msg, "invalid_request".to_string())),
                ),
                services::admin::AdminError::ServiceNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(msg, "not_found".to_string())),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to update service".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;
    Ok(ResponseJson(s.into()))
}

/// Create admin access token (Admin only)
///
/// Creates an access token for admin users with customizable expiration time, IP address, and user agent.
/// This is typically used by billing services and other automated systems that need access to admin endpoints.
///
/// **Security Note:** These tokens can have very long expiration times and should be used with caution.
/// Store them securely and rotate them regularly.
#[utoipa::path(
    post,
    path = "/v1/admin/access-tokens",
    tag = "Admin",
    request_body = CreateAdminAccessTokenRequest,
    responses(
        (status = 200, description = "Admin access token created successfully", body = AdminAccessTokenResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn create_admin_access_token(
    State(app_state): State<AdminAppState>,
    Extension(admin_user): Extension<AdminUser>, // Require admin auth
    headers: HeaderMap,
    Json(request_body): Json<CreateAdminAccessTokenRequest>,
) -> Result<ResponseJson<AdminAccessTokenResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let user_agent = headers
        .get("User-Agent")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    debug!(
        "Creating admin access token for user: {} with {} hours expiration; (User-Agent: {:?})",
        admin_user.0.email, request_body.expires_in_hours, user_agent
    );

    // Validate expiration time (must be positive)
    if request_body.expires_in_hours <= 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "expires_in_hours must be a positive number".to_string(),
                "invalid_request".to_string(),
            )),
        ));
    }

    // Create admin access token directly in database
    let expires_at = Utc::now() + chrono::Duration::hours(request_body.expires_in_hours);

    match app_state
        .admin_access_token_repository
        .create(
            admin_user.0.id,
            request_body.name,
            request_body.reason,
            expires_at,
            user_agent,
        )
        .await
    {
        Ok((admin_token, access_token)) => {
            debug!(
                "Admin access token created successfully for user: {}",
                admin_user.0.email
            );

            let response = AdminAccessTokenResponse {
                id: admin_token.id.to_string(),
                access_token,
                created_by_user_id: admin_user.0.id.to_string(),
                created_at: admin_token.created_at,
                expires_at: admin_token.expires_at,
                name: admin_token.name,
                reason: admin_token.creation_reason,
            };

            Ok(ResponseJson(response))
        }
        Err(e) => {
            error!("Failed to create admin access token");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to create admin access token: {e}"),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// List admin access tokens (Admin only)
///
/// Retrieves a paginated list of all admin access tokens in the system.
/// Only authenticated admins can access this endpoint.
#[utoipa::path(
    get,
    path = "/v1/admin/access-tokens",
    tag = "Admin",
    params(
        ("limit" = Option<i64>, Query, description = "Number of records to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Number of records to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Admin access tokens retrieved successfully"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_admin_access_tokens(
    State(app_state): State<AdminAppState>,
    Extension(admin_user): Extension<AdminUser>, // Require admin auth
    axum::extract::Query(params): axum::extract::Query<ListUsersQueryParams>,
) -> Result<ResponseJson<serde_json::Value>, (StatusCode, ResponseJson<ErrorResponse>)> {
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    debug!(
        "List admin access tokens request with limit={}, offset={} by admin: {}",
        params.limit, params.offset, admin_user.0.email
    );

    match app_state
        .admin_access_token_repository
        .list(params.limit, params.offset)
        .await
    {
        Ok(tokens) => {
            let total = app_state
                .admin_access_token_repository
                .count()
                .await
                .unwrap_or(0);

            let response = serde_json::json!({
                "data": tokens,
                "limit": params.limit,
                "offset": params.offset,
                "total": total
            });

            Ok(ResponseJson(response))
        }
        Err(e) => {
            error!("Failed to list admin access tokens");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to list admin access tokens: {e}"),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Delete admin access token (Admin only)
///
/// Revokes an admin access token by setting it as inactive.
/// Only authenticated admins can perform this operation.
#[utoipa::path(
    delete,
    path = "/v1/admin/access-tokens/{token_id}",
    tag = "Admin",
    request_body = DeleteAdminAccessTokenRequest,
    params(
        ("token_id" = String, Path, description = "ID of the admin access token to revoke")
    ),
    responses(
        (status = 200, description = "Admin access token revoked successfully"),
        (status = 404, description = "Admin access token not found", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn delete_admin_access_token(
    State(app_state): State<AdminAppState>,
    Path(token_id): Path<String>,
    Extension(admin_user): Extension<AdminUser>, // Require admin auth
    Json(request): Json<DeleteAdminAccessTokenRequest>,
) -> Result<ResponseJson<serde_json::Value>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Delete admin access token request for token_id: {} by admin: {}",
        token_id, admin_user.0.email
    );

    // Parse token ID
    let token_uuid = uuid::Uuid::parse_str(&token_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid token ID format".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Revoke the token
    match app_state
        .admin_access_token_repository
        .revoke(token_uuid, admin_user.0.id, request.reason)
        .await
    {
        Ok(true) => {
            debug!(
                "Admin access token {} revoked successfully by admin: {}",
                token_id, admin_user.0.email
            );

            let response = serde_json::json!({
                "message": "Admin access token revoked successfully",
                "token_id": token_id,
                "revoked_by": admin_user.0.email,
                "revoked_at": chrono::Utc::now().to_rfc3339()
            });

            Ok(ResponseJson(response))
        }
        Ok(false) => {
            debug!(
                "Admin access token {} not found or already revoked",
                token_id
            );
            Err((
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    "Admin access token not found or already revoked".to_string(),
                    "token_not_found".to_string(),
                )),
            ))
        }
        Err(e) => {
            error!("Failed to revoke admin access token");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to revoke admin access token: {e}"),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ListUsersQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub include_organizations: bool,
    pub search: Option<String>,
    pub is_active: Option<bool>,
    pub search_by_name: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ListOrganizationsQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

#[derive(Debug, serde::Deserialize)]
pub struct ListInvitationEmailDeliveriesQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    pub organization_id: Option<Uuid>,
    pub recipient_email: Option<String>,
    pub email_status: Option<crate::models::InvitationEmailStatus>,
    pub invitation_status: Option<crate::models::InvitationStatus>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ListModelsQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub include_inactive: bool,
}

#[derive(Debug, serde::Deserialize)]
pub struct ModelHistoryQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

#[derive(Debug, serde::Deserialize)]
pub struct OrgLimitsHistoryQueryParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

#[derive(Debug, serde::Deserialize)]
pub struct MetricsQueryParams {
    /// Start of time range (ISO 8601 format). Defaults to 30 days ago.
    pub start: Option<String>,
    /// End of time range (ISO 8601 format). Defaults to now.
    pub end: Option<String>,
}

/// Get organization metrics (Admin only)
///
/// Returns usage metrics for an organization including summary totals,
/// and breakdowns by workspace, API key, and model.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/metrics",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID to get metrics for"),
        ("start" = Option<String>, Query, description = "Start of time range (ISO 8601). Defaults to 30 days ago."),
        ("end" = Option<String>, Query, description = "End of time range (ISO 8601). Defaults to now.")
    ),
    responses(
        (status = 200, description = "Organization metrics retrieved successfully"),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_metrics(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Query(params): Query<MetricsQueryParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<
    ResponseJson<services::admin::OrganizationMetrics>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    debug!(
        "Get organization metrics request for org_id: {}, start: {:?}, end: {:?}",
        org_id, params.start, params.end
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

    // Parse time range with defaults
    let end = params
        .end
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    let start = params
        .start
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| end - Duration::days(30));

    // Get metrics from analytics service
    let metrics = app_state
        .analytics_service
        .get_organization_metrics(organization_id, start, end)
        .await
        .map_err(|e| {
            error!("Failed to get organization metrics, error: {:?}", e);
            match e {
                services::admin::AdminError::OrganizationNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        msg,
                        "organization_not_found".to_string(),
                    )),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        format!("Failed to retrieve metrics: {e}"),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    Ok(ResponseJson(metrics))
}

/// Get platform-wide metrics for admin dashboards (Admin only)
///
/// Returns aggregated metrics across all organizations including:
/// - Total users and organizations
/// - Total requests and revenue
/// - Top models by usage
/// - Top organizations by spend
#[utoipa::path(
    get,
    path = "/v1/admin/platform/metrics",
    tag = "Admin",
    params(
        ("start" = Option<String>, Query, description = "Start of time range (ISO 8601). Defaults to 30 days ago."),
        ("end" = Option<String>, Query, description = "End of time range (ISO 8601). Defaults to now.")
    ),
    responses(
        (status = 200, description = "Platform metrics retrieved successfully", body = services::admin::PlatformMetrics),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_platform_metrics(
    State(app_state): State<AdminAppState>,
    Query(params): Query<MetricsQueryParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<ResponseJson<services::admin::PlatformMetrics>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!(
        "Get platform metrics request, start: {:?}, end: {:?}",
        params.start, params.end
    );

    let (start, end) = crate::routes::common::parse_metrics_range(
        params.start.as_deref(),
        params.end.as_deref(),
        None,
        0,
    )?;

    // Get platform metrics from analytics service
    let metrics = app_state
        .analytics_service
        .get_platform_metrics(start, end)
        .await
        .map_err(|e| {
            error!("Failed to get platform metrics, error: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to retrieve platform metrics: {e}"),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok(ResponseJson(metrics))
}

/// Get platform-wide time series for admin dashboards (Admin only)
///
/// Returns per-bucket requests, tokens, cost (paid/granted + verifiable/external splits),
/// active organizations, and new signups for growth/mix trend charts.
#[utoipa::path(
    get,
    path = "/v1/admin/platform/metrics/timeseries",
    tag = "Admin",
    params(
        ("start" = Option<String>, Query, description = "Start of time range (ISO 8601). Defaults to 30 days ago."),
        ("end" = Option<String>, Query, description = "End of time range (ISO 8601). Defaults to now."),
        ("granularity" = Option<String>, Query, description = "Time granularity: hour, day (default), week, or month")
    ),
    responses(
        (status = 200, description = "Platform time series retrieved successfully", body = services::admin::PlatformTimeSeriesMetrics),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_platform_timeseries(
    State(app_state): State<AdminAppState>,
    Query(params): Query<TimeSeriesQueryParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<
    ResponseJson<services::admin::PlatformTimeSeriesMetrics>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    debug!(
        "Get platform timeseries request, start: {:?}, end: {:?}, granularity: {}",
        params.start, params.end, params.granularity
    );

    // Validate granularity (platform supports month in addition to hour/day/week)
    let granularity = match params.granularity.as_str() {
        "hour" | "day" | "week" | "month" => params.granularity.as_str(),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Invalid granularity. Must be 'hour', 'day', 'week', or 'month'".to_string(),
                    "invalid_granularity".to_string(),
                )),
            ))
        }
    };

    // Cap `hour` granularity to 31 days to avoid unbounded bucket counts.
    let (start, end) = crate::routes::common::parse_metrics_range(
        params.start.as_deref(),
        params.end.as_deref(),
        Some(granularity),
        31,
    )?;

    let metrics = app_state
        .analytics_service
        .get_platform_timeseries(start, end, granularity)
        .await
        .map_err(|e| {
            error!("Failed to get platform timeseries, error: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to retrieve platform timeseries: {e}"),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok(ResponseJson(metrics))
}

/// Get the platform billing summary (Admin only)
///
/// Credit LIMITS (caps) and consumption — NOT payments/cash. Returns active paid/grant
/// credit limits, total consumed, paying/granted org counts, and a breakdown by funding
/// source. Real money-in lives in the billing service, not cloud-api.
#[utoipa::path(
    get,
    path = "/v1/admin/platform/billing-summary",
    tag = "Admin",
    responses(
        (status = 200, description = "Billing summary retrieved successfully", body = services::admin::BillingSummary),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_billing_summary(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<ResponseJson<services::admin::BillingSummary>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!("Get platform billing summary request");

    let summary = app_state
        .analytics_service
        .get_billing_summary()
        .await
        .map_err(|e| {
            error!("Failed to get billing summary, error: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to retrieve billing summary: {e}"),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok(ResponseJson(summary))
}

#[derive(Debug, serde::Deserialize)]
pub struct ModelRevenueQueryParams {
    /// Start of time range (ISO 8601). Defaults to 30 days ago.
    pub start: Option<String>,
    /// End of time range (ISO 8601). Defaults to now.
    pub end: Option<String>,
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    /// Filter by verifiable (TEE) models only / non-verifiable only.
    pub verifiable: Option<bool>,
    /// Filter by provider type ("vllm" or "external").
    pub provider_type: Option<String>,
    /// Case-insensitive substring match on model name.
    pub model_search: Option<String>,
    /// Sort key: "revenue" (default), "requests", or "tokens".
    pub sort: Option<String>,
}

/// Get the per-model consumption ranking (Admin only)
///
/// Models for the selected period ranked by consumed cost, with requests, tokens,
/// unique orgs, verifiable flag, provider type, and latency. Paginated and filterable.
#[utoipa::path(
    get,
    path = "/v1/admin/platform/model-revenue",
    tag = "Admin",
    params(
        ("start" = Option<String>, Query, description = "Start of time range (ISO 8601). Defaults to 30 days ago."),
        ("end" = Option<String>, Query, description = "End of time range (ISO 8601). Defaults to now."),
        ("limit" = Option<i64>, Query, description = "Page size (1-1000, default 100)"),
        ("offset" = Option<i64>, Query, description = "Page offset (default 0)"),
        ("verifiable" = Option<bool>, Query, description = "Filter to verifiable (true) or non-verifiable (false) models"),
        ("provider_type" = Option<String>, Query, description = "Filter by provider type (e.g. vllm, external)"),
        ("sort" = Option<String>, Query, description = "Sort: revenue (default), requests, tokens")
    ),
    responses(
        (status = 200, description = "Model revenue retrieved successfully", body = services::admin::ModelRevenueReport),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_model_revenue(
    State(app_state): State<AdminAppState>,
    Query(params): Query<ModelRevenueQueryParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<
    ResponseJson<services::admin::ModelRevenueReport>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    debug!(
        "Get platform model revenue request, start: {:?}, end: {:?}, limit: {}, offset: {}",
        params.start, params.end, params.limit, params.offset
    );
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;
    let (start, end) = crate::routes::common::parse_metrics_range(
        params.start.as_deref(),
        params.end.as_deref(),
        None,
        0,
    )?;
    let sort = services::admin::RevenueSort::from_query(params.sort.as_deref())
        .map_err(|m| bad_request(m, "invalid_parameter"))?;
    if let Some(pt) = params.provider_type.as_deref() {
        if pt != "vllm" && pt != "external" {
            return Err(bad_request(
                format!("invalid provider_type '{pt}'; expected 'vllm' or 'external'"),
                "invalid_parameter",
            ));
        }
    }

    let report = app_state
        .analytics_service
        .get_model_revenue(services::admin::ModelRevenueQuery {
            start,
            end,
            verifiable: params.verifiable,
            provider_type: params.provider_type,
            model_search: params.model_search,
            sort,
            limit: params.limit,
            offset: params.offset,
        })
        .await
        .map_err(|e| {
            error!("Failed to get model revenue, error: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to retrieve model revenue: {e}"),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok(ResponseJson(report))
}

#[derive(Debug, serde::Deserialize)]
pub struct OrgRevenueQueryParams {
    /// Start of time range (ISO 8601). Defaults to 30 days ago.
    pub start: Option<String>,
    /// End of time range (ISO 8601). Defaults to now.
    pub end: Option<String>,
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    /// Filter to current paying (true) / non-paying (false) orgs.
    pub paying: Option<bool>,
    /// Case-insensitive substring match on organization name.
    pub search: Option<String>,
    /// Sort key: "revenue" (default), "requests", or "tokens".
    pub sort: Option<String>,
}

/// Get the per-organization consumption ranking (Admin only)
///
/// Organizations with usage in the selected period ranked by consumed cost, with the
/// verifiable/external split, requests, tokens, models used, a current paying flag, and
/// last-usage timestamp. Paginated and filterable — full attribution of usage/spend per org.
#[utoipa::path(
    get,
    path = "/v1/admin/platform/org-revenue",
    tag = "Admin",
    params(
        ("start" = Option<String>, Query, description = "Start of time range (ISO 8601). Defaults to 30 days ago."),
        ("end" = Option<String>, Query, description = "End of time range (ISO 8601). Defaults to now."),
        ("limit" = Option<i64>, Query, description = "Page size (1-1000, default 100)"),
        ("offset" = Option<i64>, Query, description = "Page offset (default 0)"),
        ("paying" = Option<bool>, Query, description = "Filter to current paying (true) / non-paying (false) orgs"),
        ("sort" = Option<String>, Query, description = "Sort: revenue (default), requests, tokens")
    ),
    responses(
        (status = 200, description = "Org revenue retrieved successfully", body = services::admin::OrgRevenueReport),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_org_revenue(
    State(app_state): State<AdminAppState>,
    Query(params): Query<OrgRevenueQueryParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<
    ResponseJson<services::admin::OrgRevenueReport>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    debug!(
        "Get platform org revenue request, start: {:?}, end: {:?}, limit: {}, offset: {}",
        params.start, params.end, params.limit, params.offset
    );
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;
    let (start, end) = crate::routes::common::parse_metrics_range(
        params.start.as_deref(),
        params.end.as_deref(),
        None,
        0,
    )?;
    let sort = services::admin::RevenueSort::from_query(params.sort.as_deref())
        .map_err(|m| bad_request(m, "invalid_parameter"))?;

    let report = app_state
        .analytics_service
        .get_org_revenue(services::admin::OrgRevenueQuery {
            start,
            end,
            paying: params.paying,
            search: params.search,
            sort,
            limit: params.limit,
            offset: params.offset,
        })
        .await
        .map_err(|e| {
            error!("Failed to get org revenue, error: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to retrieve org revenue: {e}"),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok(ResponseJson(report))
}

/// Get the platform infrastructure / fleet burn summary (Admin only)
///
/// Fetches the live host list, counts active/idle hosts, and computes the monthly/daily
/// GPU burn rate from the configured cost-per-host. Degrades gracefully (stale=true) if
/// the host inventory is unreachable.
#[utoipa::path(
    get,
    path = "/v1/admin/platform/infra-summary",
    tag = "Admin",
    responses(
        (status = 200, description = "Infra summary retrieved successfully", body = services::admin::InfraSummary),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_infra_summary(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<ResponseJson<services::admin::InfraSummary>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!("Get platform infra summary request");
    let summary = app_state.infra_service.get_infra_summary().await;
    Ok(ResponseJson(summary))
}

#[derive(Debug, serde::Deserialize)]
pub struct TimeSeriesQueryParams {
    /// Start of time range (ISO 8601 format). Defaults to 30 days ago.
    pub start: Option<String>,
    /// End of time range (ISO 8601 format). Defaults to now.
    pub end: Option<String>,
    /// Granularity: "hour", "day" (default), or "week"
    #[serde(default = "default_granularity")]
    pub granularity: String,
}

fn default_granularity() -> String {
    "day".to_string()
}

#[derive(Debug, serde::Deserialize)]
pub struct ModelConsumptionTimeseriesParams {
    pub start: Option<String>,
    pub end: Option<String>,
    #[serde(default = "default_granularity")]
    pub granularity: String,
    /// Number of top models returned as separate series (rest → "Other"). Max 20.
    #[serde(default = "default_top_n")]
    pub top_n: i64,
}

fn default_top_n() -> i64 {
    15
}

#[derive(Debug, serde::Deserialize)]
pub struct PerformanceTimeseriesParams {
    pub start: Option<String>,
    pub end: Option<String>,
    #[serde(default = "default_granularity")]
    pub granularity: String,
    /// Optional exact model name filter (platform-wide if omitted)
    pub model_name: Option<String>,
}

/// Get per-model consumption timeseries (Admin only)
///
/// Returns consumed cost, requests, and tokens per time bucket broken down by model.
/// Top N models by total period cost are returned as separate series; all others
/// are collapsed into a single "Other" bucket. Model labels use the current canonical
/// name from the models table (joined on model_id, rename-safe).
///
/// The query does **not** zero-fill missing (bucket, model) pairs — the frontend
/// must impute zeros for models absent from a bucket.
///
/// Consumed cost = metered inference cost, NOT cash revenue (includes grant credits).
#[utoipa::path(
    get,
    path = "/v1/admin/platform/model-consumption-timeseries",
    tag = "Admin",
    params(
        ("start" = Option<String>, Query, description = "Start of time range (ISO 8601). Defaults to 30 days ago."),
        ("end" = Option<String>, Query, description = "End of time range (ISO 8601). Defaults to now."),
        ("granularity" = Option<String>, Query, description = "Time granularity: hour (≤31d), day (≤366d, default), week (≤3y), month (≤5y)"),
        ("top_n" = Option<i64>, Query, description = "Top-N models to return as separate series (1-20, default 15); rest → 'Other'")
    ),
    responses(
        (status = 200, description = "Model consumption timeseries retrieved successfully", body = services::admin::ModelConsumptionTimeseries),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(("session_token" = []))
)]
pub async fn get_model_consumption_timeseries(
    State(app_state): State<AdminAppState>,
    Query(params): Query<ModelConsumptionTimeseriesParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<
    ResponseJson<services::admin::ModelConsumptionTimeseries>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    let top_n = params.top_n.clamp(1, 20);

    let granularity = crate::routes::common::allowlisted_date_trunc(&params.granularity)?;
    let (start, end) = crate::routes::common::parse_metrics_range(
        params.start.as_deref(),
        params.end.as_deref(),
        Some(granularity),
        31,
    )?;

    let result = app_state
        .analytics_service
        .get_model_consumption_timeseries(services::admin::ModelConsumptionTimeseriesQuery {
            start,
            end,
            granularity: granularity.to_string(),
            top_n,
        })
        .await
        .map_err(|e| {
            error!("Failed to get model consumption timeseries: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to retrieve model consumption timeseries: {e}"),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok(ResponseJson(result))
}

/// Get platform-wide performance timeseries (Admin only)
///
/// Returns per-time-bucket: request count, token throughput (total and output-only),
/// TTFT percentiles (p50/p95/p99), and error rate.
///
/// **TTFT percentiles cover streaming requests only** (`ttft_ms IS NOT NULL`). The
/// `ttft_sample_count` field in each bucket exposes the denominator so callers can
/// compute coverage fraction (`ttft_sample_count / requests`).
///
/// **Error rate** = `stop_reason IN ('provider_error','timeout')` / requests with a
/// recorded `stop_reason`. Pre-V0037 rows (stop_reason IS NULL) are excluded from both
/// numerator and denominator.
#[utoipa::path(
    get,
    path = "/v1/admin/platform/performance-timeseries",
    tag = "Admin",
    params(
        ("start" = Option<String>, Query, description = "Start of time range (ISO 8601). Defaults to 30 days ago."),
        ("end" = Option<String>, Query, description = "End of time range (ISO 8601). Defaults to now."),
        ("granularity" = Option<String>, Query, description = "Time granularity: hour (≤31d), day (≤366d, default), week (≤3y), month (≤5y)"),
        ("model_name" = Option<String>, Query, description = "Filter to a single model name (platform-wide if omitted)")
    ),
    responses(
        (status = 200, description = "Performance timeseries retrieved successfully", body = services::admin::PerformanceTimeseries),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(("session_token" = []))
)]
pub async fn get_performance_timeseries(
    State(app_state): State<AdminAppState>,
    Query(params): Query<PerformanceTimeseriesParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<
    ResponseJson<services::admin::PerformanceTimeseries>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    let granularity = crate::routes::common::allowlisted_date_trunc(&params.granularity)?;
    let (start, end) = crate::routes::common::parse_metrics_range(
        params.start.as_deref(),
        params.end.as_deref(),
        Some(granularity),
        31,
    )?;

    let result = app_state
        .analytics_service
        .get_performance_timeseries(services::admin::PerformanceTimeseriesQuery {
            start,
            end,
            granularity: granularity.to_string(),
            model_name: params.model_name,
        })
        .await
        .map_err(|e| {
            error!("Failed to get performance timeseries: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to retrieve performance timeseries: {e}"),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok(ResponseJson(result))
}

/// Query params for the revenue density endpoint
#[derive(Debug, serde::Deserialize)]
pub struct RevenueDensityParams {
    pub start: Option<String>,
    pub end: Option<String>,
}

/// Get revenue density percentiles (Admin only)
///
/// Buckets usage into 1-minute windows, computes revenue/second per bucket,
/// then returns P50/P95/P99/peak over active buckets — platform-wide and per model.
/// Use the annualized figures to estimate potential revenue if a given demand
/// rate were sustained continuously.
pub async fn get_revenue_density(
    State(app_state): State<AdminAppState>,
    Query(params): Query<RevenueDensityParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<
    ResponseJson<services::admin::RevenueDensityReport>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    // Default to last 30 days; cap at 90 days (43k–129k minute-buckets).
    let (start, end) = crate::routes::common::parse_metrics_range(
        params.start.as_deref(),
        params.end.as_deref(),
        None,
        90,
    )?;

    let result = app_state
        .analytics_service
        .get_revenue_density(services::admin::RevenueDensityQuery { start, end })
        .await
        .map_err(|e| {
            error!("Failed to get revenue density: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to retrieve revenue density: {e}"),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    Ok(ResponseJson(result))
}

/// Get time series metrics for an organization (Admin only)
///
/// Returns daily/weekly/hourly aggregations for charting:
/// requests, tokens, and cost per time period.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/metrics/timeseries",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID to get metrics for"),
        ("start" = Option<String>, Query, description = "Start of time range (ISO 8601). Defaults to 30 days ago."),
        ("end" = Option<String>, Query, description = "End of time range (ISO 8601). Defaults to now."),
        ("granularity" = Option<String>, Query, description = "Time granularity: hour, day (default), or week")
    ),
    responses(
        (status = 200, description = "Time series metrics retrieved successfully"),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_timeseries(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Query(params): Query<TimeSeriesQueryParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<
    ResponseJson<services::admin::TimeSeriesMetrics>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    debug!(
        "Get organization timeseries request for org_id: {}, start: {:?}, end: {:?}, granularity: {}",
        org_id, params.start, params.end, params.granularity
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

    // Validate granularity
    let granularity = match params.granularity.as_str() {
        "hour" | "day" | "week" => params.granularity.as_str(),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Invalid granularity. Must be 'hour', 'day', or 'week'".to_string(),
                    "invalid_granularity".to_string(),
                )),
            ))
        }
    };

    // Parse time range — hard error on bad input, 366-day cap for non-hour granularities
    let (start, end) = crate::routes::common::parse_metrics_range(
        params.start.as_deref(),
        params.end.as_deref(),
        Some(granularity),
        31,
    )?;

    // Get timeseries from analytics service
    let metrics = app_state
        .analytics_service
        .get_organization_timeseries(organization_id, start, end, granularity)
        .await
        .map_err(|e| {
            error!("Failed to get organization timeseries, error: {:?}", e);
            match e {
                services::admin::AdminError::OrganizationNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        msg,
                        "organization_not_found".to_string(),
                    )),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        format!("Failed to retrieve timeseries metrics: {e}"),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    Ok(ResponseJson(metrics))
}

/// Update organization concurrent request limit (Admin only)
///
/// Updates the maximum concurrent requests allowed per model for an organization.
/// Set to null to use the default limit (64).
/// Changes take effect within 5 minutes due to caching.
#[utoipa::path(
    patch,
    path = "/v1/admin/organizations/{org_id}/concurrent-limit",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "The organization's ID (as a UUID)")
    ),
    request_body = UpdateOrganizationConcurrentLimitRequest,
    responses(
        (status = 200, description = "Concurrent limit updated successfully", body = UpdateOrganizationConcurrentLimitResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn update_organization_concurrent_limit(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Extension(_admin_user): Extension<AdminUser>,
    ResponseJson(request): ResponseJson<UpdateOrganizationConcurrentLimitRequest>,
) -> Result<
    ResponseJson<UpdateOrganizationConcurrentLimitResponse>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    debug!(
        "Update organization concurrent limit request for org_id: {}, limit: {:?}",
        org_id, request.concurrent_limit
    );

    // Parse organization ID
    let org_uuid = uuid::Uuid::parse_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID format".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Update concurrent limit via admin service
    app_state
        .admin_service
        .update_organization_concurrent_limit(org_uuid, request.concurrent_limit)
        .await
        .map_err(|e| {
            error!("Failed to update organization concurrent limit");
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
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to update concurrent limit".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let response = UpdateOrganizationConcurrentLimitResponse {
        organization_id: org_id.clone(),
        concurrent_limit: request.concurrent_limit,
        updated_at: Utc::now().to_rfc3339(),
    };

    Ok(ResponseJson(response))
}

/// Get organization concurrent request limit (Admin only)
///
/// Returns the current concurrent request limit for an organization.
/// If no custom limit is set, returns null for concurrent_limit and the default (64) for effective_limit.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/concurrent-limit",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "The organization's ID (as a UUID)")
    ),
    responses(
        (status = 200, description = "Concurrent limit retrieved successfully", body = GetOrganizationConcurrentLimitResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_concurrent_limit(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<
    ResponseJson<GetOrganizationConcurrentLimitResponse>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    debug!(
        "Get organization concurrent limit request for org_id: {}",
        org_id
    );

    // Parse organization ID
    let org_uuid = uuid::Uuid::parse_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID format".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Get concurrent limit via admin service
    let concurrent_limit = app_state
        .admin_service
        .get_organization_concurrent_limit(org_uuid)
        .await
        .map_err(|e| {
            error!("Failed to get organization concurrent limit");
            match e {
                services::admin::AdminError::OrganizationNotFound(msg) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        msg,
                        "organization_not_found".to_string(),
                    )),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to get concurrent limit".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    // Filter out zero values (shouldn't happen due to validation, but defensive)
    let concurrent_limit = concurrent_limit.filter(|&limit| limit > 0);
    let effective_limit =
        concurrent_limit.unwrap_or(services::completions::ports::DEFAULT_CONCURRENT_LIMIT);

    let response = GetOrganizationConcurrentLimitResponse {
        organization_id: org_id,
        concurrent_limit,
        effective_limit,
    };

    Ok(ResponseJson(response))
}

#[cfg(test)]
mod deprecation_date_tests {
    use super::{format_deprecation_date, parse_deprecation_date};

    #[test]
    fn date_only_defaults_to_13_00_utc() {
        // OpenRouter spec: "Date-only values default to 13:00 UTC on that date."
        let dt = parse_deprecation_date("2030-01-01").expect("date-only must parse");
        assert_eq!(format_deprecation_date(&dt), "2030-01-01T13:00:00Z");
    }

    #[test]
    fn explicit_utc_hour_round_trips() {
        // Spec example: 2025-06-01T15:00:00Z is accepted as-is.
        let dt = parse_deprecation_date("2025-06-01T15:00:00Z").expect("must parse");
        assert_eq!(format_deprecation_date(&dt), "2025-06-01T15:00:00Z");
    }

    #[test]
    fn zero_offset_plus_00_00_is_accepted_as_utc() {
        // `+00:00` denotes the same instant as `Z`; with whole-hour
        // minutes/seconds it is accepted and round-trips to the `Z` form.
        let dt = parse_deprecation_date("2025-06-01T15:00:00+00:00").expect("must parse");
        assert_eq!(format_deprecation_date(&dt), "2025-06-01T15:00:00Z");
    }

    #[test]
    fn sub_hour_precision_is_rejected() {
        // Off-hour datetimes must be rejected (not truncated): truncation would
        // deprecate the model earlier than the requested instant.
        assert!(parse_deprecation_date("2025-06-01T15:47:33Z").is_none());
        // Non-zero minutes alone are also rejected.
        assert!(parse_deprecation_date("2025-06-01T15:30:00Z").is_none());
    }

    #[test]
    fn non_utc_offset_is_rejected() {
        // 15:30+02:00 is not a whole-hour UTC instant; reject it.
        assert!(parse_deprecation_date("2025-06-01T15:30:00+02:00").is_none());
        // Even a whole-hour wall-clock time in a non-UTC offset is rejected:
        // 15:00+02:00 == 13:00 UTC, which the caller did not write.
        assert!(parse_deprecation_date("2025-06-01T15:00:00+02:00").is_none());
    }

    #[test]
    fn invalid_input_returns_none() {
        assert!(parse_deprecation_date("not-a-date").is_none());
        assert!(parse_deprecation_date("2025-13-01").is_none());
    }
}

#[cfg(test)]
mod openrouter_slug_tests {
    use super::is_valid_openrouter_slug;

    #[test]
    fn accepts_canonical_openrouter_slugs() {
        // The real slugs we need to back-fill (per the OpenRouter for-providers
        // spec) must all validate.
        for slug in [
            "z-ai/glm-5.1",
            "deepseek/deepseek-v4-flash",
            "google/gemma-4-31b-it",
            "qwen/qwen3.5-122b-a10b",
            "qwen/qwen3.6-27b",
            "qwen/qwen3.6-35b-a3b",
            "qwen/qwen3-vl-30b-a3b-instruct",
            "qwen/qwen3-30b-a3b-instruct-2507",
            "openai/gpt-oss-120b",
            "a/b",         // minimal single-char segments
            "a.b_c-d/e.f", // all interior punctuation classes
        ] {
            assert!(
                is_valid_openrouter_slug(slug),
                "expected '{slug}' to be a valid OpenRouter slug"
            );
        }
    }

    #[test]
    fn rejects_malformed_slugs() {
        for slug in [
            "",              // empty
            "glm-5.1",       // missing author/ segment
            "Z-AI/glm-5.1",  // uppercase
            "z-ai/GLM-5.1",  // uppercase in slug
            "z-ai/",         // empty slug segment
            "/glm-5.1",      // empty author segment
            "z-ai/glm/5.1",  // more than one separator
            "-z-ai/glm-5.1", // leading punctuation
            "z-ai-/glm-5.1", // trailing punctuation on author
            "z-ai/glm-5.1-", // trailing punctuation on slug
            "z-ai/.glm",     // leading punctuation on slug
            "z ai/glm-5.1",  // space is not allowed
        ] {
            assert!(
                !is_valid_openrouter_slug(slug),
                "expected '{slug}' to be rejected"
            );
        }
    }
}
