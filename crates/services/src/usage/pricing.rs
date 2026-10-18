//! Turns a usage request plus the model's catalog pricing into the amounts a
//! usage row is recorded at.

use super::{
    compute_profiled_text_cost, compute_token_cost_with_cache_write, ports, CacheWriteBilling,
    ModelPricing, RecordUsageServiceRequest, TextServiceTier, UsageError,
};
use crate::metrics::{
    consts::{get_environment, METRIC_BILLING_PRICING_FALLBACK, TAG_ENVIRONMENT, TAG_MODEL},
    MetricsServiceTrait,
};

/// Amounts and pricing metadata for one usage row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PricedUsage {
    /// Cache-read tokens clamped to `0..=input_tokens`.
    pub cache_read_tokens: i32,
    /// Cache-write tokens persisted on the row: the profiled subset for
    /// profiled text models, the legacy subset otherwise.
    pub cache_write_tokens: i32,
    pub input_cost: i64,
    pub output_cost: i64,
    pub total_cost: i64,
    pub billing_details: Option<serde_json::Value>,
    pub service_tier: Option<String>,
    pub context_band: Option<String>,
}

/// Price `request` against `model`, then apply any reporter discount.
///
/// Images bill `image_count × cost_per_image`; audio bills duration seconds
/// (carried in `input_tokens`) at the input rate; rerank, embedding, score and
/// privacy classify bill input tokens at the input rate; everything else is
/// token-priced, through the exact text profile when the model has one (which
/// may emit the critical pricing-fallback metric) or the cache-aware token
/// formula otherwise. Cache pricing never applies to the non-token types.
pub(super) fn price_usage(
    request: &RecordUsageServiceRequest,
    model: &ModelPricing,
    metrics: &dyn MetricsServiceTrait,
) -> Result<PricedUsage, UsageError> {
    // Normalize cache_read_tokens to satisfy invariant: 0 <= cache_read_tokens <= input_tokens.
    // Ensures cost computation and persisted usage stay consistent across all callers
    // (API, provider parsing, etc.).
    let cache_read_tokens = request.cache_read_tokens.min(request.input_tokens).max(0);
    let cache_write = request.cache_write.map(|write| CacheWriteBilling {
        tokens: write
            .tokens
            .min(request.input_tokens.saturating_sub(cache_read_tokens))
            .max(0),
        cost_per_token: write.cost_per_token,
    });
    let legacy_cache_write_tokens = cache_write.map(|write| write.tokens).unwrap_or(0);
    let profiled_cache_write_tokens = request
        .profiled_cache_write_tokens
        .min(request.input_tokens.saturating_sub(cache_read_tokens))
        .max(0);

    // Unpriced by token class: only the costs differ by type.
    let flat = |input_cost: i64, output_cost: i64, total_cost: i64| PricedUsage {
        cache_read_tokens,
        cache_write_tokens: legacy_cache_write_tokens,
        input_cost,
        output_cost,
        total_cost,
        billing_details: None,
        service_tier: None,
        context_band: None,
    };

    let priced = match request.inference_type {
        ports::InferenceType::ImageGeneration | ports::InferenceType::ImageEdit => {
            // For image-based operations: use image_count and cost_per_image
            let image_count = request.image_count.unwrap_or(0);
            // Use checked arithmetic to prevent integer overflow in billing-critical path
            let image_cost = (image_count as i64)
                .checked_mul(model.cost_per_image)
                .ok_or_else(|| {
                    UsageError::CostCalculationOverflow(format!(
                        "Image cost calculation overflow: {} images * {} cost_per_image",
                        image_count, model.cost_per_image
                    ))
                })?;
            flat(0, image_cost, image_cost)
        }
        ports::InferenceType::AudioTranscription => {
            // For audio transcription: bill by duration in seconds (stored in input_tokens).
            // Cache pricing is intentionally NOT applied for audio transcription, even if
            // cache_read_cost_per_token is configured on the model.
            // input_tokens contains the audio duration rounded up to nearest second
            let duration_cost = (request.input_tokens as i64)
                .checked_mul(model.input_cost_per_token)
                .ok_or_else(|| {
                    UsageError::CostCalculationOverflow(format!(
                        "Audio transcription cost calculation overflow: {} seconds * {} cost_per_token",
                        request.input_tokens, model.input_cost_per_token
                    ))
                })?;
            flat(duration_cost, 0, duration_cost)
        }
        ports::InferenceType::Rerank
        | ports::InferenceType::Embedding
        | ports::InferenceType::Score
        | ports::InferenceType::PrivacyClassify => {
            // For rerank/embedding/score/privacy_classify: use input tokens as the billing unit.
            // Cache pricing is intentionally NOT applied, even if
            // cache_read_cost_per_token is configured on the model.
            // Use checked arithmetic to prevent integer overflow in billing-critical path
            let cost = (request.input_tokens as i64)
                .checked_mul(model.input_cost_per_token)
                .ok_or_else(|| {
                    UsageError::CostCalculationOverflow(format!(
                        "{} cost calculation overflow: {} tokens * {} cost_per_token",
                        request.inference_type, request.input_tokens, model.input_cost_per_token
                    ))
                })?;
            flat(cost, 0, cost)
        }
        _ => {
            // For token-based models (chat completions, etc.)
            if let Some(profile) = &model.text_pricing {
                let requested_tier = request
                    .requested_service_tier
                    .unwrap_or(TextServiceTier::Default);
                let priced = compute_profiled_text_cost(
                    request.input_tokens,
                    request.output_tokens,
                    cache_read_tokens,
                    profiled_cache_write_tokens,
                    requested_tier,
                    request.provider_service_tier.as_deref(),
                    profile,
                )?;
                if let Some(reason) = priced.snapshot.critical_fallback_reason.as_deref() {
                    let environment = get_environment();
                    let tags = [
                        format!("{}:{}", TAG_MODEL, model.model_name),
                        format!("reason:{reason}"),
                        format!("{TAG_ENVIRONMENT}:{environment}"),
                    ];
                    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
                    metrics.record_count(METRIC_BILLING_PRICING_FALLBACK, 1, &tag_refs);
                    tracing::error!(
                        model = %model.model_name,
                        reason,
                        "Critical billing pricing fallback applied"
                    );
                }
                let service_tier = Some(priced.snapshot.actual_tier.clone());
                let context_band = Some(priced.snapshot.context_band.as_str().to_string());
                let billing_details =
                    Some(serde_json::to_value(&priced.snapshot).map_err(|error| {
                        UsageError::InternalError(format!(
                            "Failed to serialize billing snapshot: {error}"
                        ))
                    })?);
                PricedUsage {
                    cache_read_tokens,
                    cache_write_tokens: profiled_cache_write_tokens,
                    input_cost: priced.cost.input_cost,
                    output_cost: priced.cost.output_cost,
                    total_cost: priced.cost.total_cost,
                    billing_details,
                    service_tier,
                    context_band,
                }
            } else {
                let cost = compute_token_cost_with_cache_write(
                    request.input_tokens,
                    request.output_tokens,
                    cache_read_tokens,
                    cache_write,
                    model,
                )?;
                flat(cost.input_cost, cost.output_cost, cost.total_cost)
            }
        }
    };

    // A reporter-supplied discount is applied after pricing so every SKU
    // (input, cache read/write, output, image) gets the same multiplier,
    // and the list amounts stay in `billing_details` for auditing.
    match request.discount {
        Some(discount) => {
            let discounted = discount.apply_to_costs(
                priced.input_cost,
                priced.output_cost,
                priced.billing_details,
            )?;
            Ok(PricedUsage {
                input_cost: discounted.input_cost,
                output_cost: discounted.output_cost,
                total_cost: discounted.total_cost,
                billing_details: Some(discounted.billing_details),
                ..priced
            })
        }
        None => Ok(priced),
    }
}
