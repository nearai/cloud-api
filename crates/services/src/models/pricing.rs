use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::ModelWithPricing;
use crate::{admin::ScheduledPricingChangeStatus, usage::TextPricingProfile};

/// Public pricing metadata only, selected alongside a canonical model. The
/// administrator's identity, reason and delivery history never enter this DTO.
#[derive(Debug, Deserialize)]
pub struct ModelPricingChange {
    pub effective_at: DateTime<Utc>,
    pub status: ScheduledPricingChangeStatus,
    pub new_input_cost_per_token: Option<i64>,
    pub new_output_cost_per_token: Option<i64>,
    pub new_cache_read_cost_per_token: Option<i64>,
    pub new_cost_per_image: Option<i64>,
    pub new_text_pricing: Option<TextPricingProfile>,
}

/// Complete future prices, in the same nano-dollar units as the current model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpcomingModelPricing {
    pub effective_at: DateTime<Utc>,
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,
    pub cache_read_cost_per_token: Option<i64>,
    pub cost_per_image: i64,
    pub text_pricing: Option<TextPricingProfile>,
}

impl UpcomingModelPricing {
    pub fn earliest_pending(
        model: &ModelWithPricing,
        changes: &[ModelPricingChange],
    ) -> Option<Self> {
        let change = changes
            .iter()
            .filter(|change| change.status == ScheduledPricingChangeStatus::Pending)
            .min_by_key(|change| change.effective_at)?;

        // Like ModelPricingScheduler, omitted fields retain the live value,
        // which may differ from the historical snapshot captured at confirm.
        // A pending row remains a notice even if its date is overdue: wall
        // clock time alone does not mean the scheduler has applied the price.
        Some(Self {
            effective_at: change.effective_at,
            input_cost_per_token: change
                .new_input_cost_per_token
                .unwrap_or(model.input_cost_per_token),
            output_cost_per_token: change
                .new_output_cost_per_token
                .unwrap_or(model.output_cost_per_token),
            cache_read_cost_per_token: change
                .new_cache_read_cost_per_token
                .or(model.cache_read_cost_per_token),
            cost_per_image: change.new_cost_per_image.unwrap_or(model.cost_per_image),
            text_pricing: change
                .new_text_pricing
                .clone()
                .or_else(|| model.text_pricing.clone()),
        })
    }
}
