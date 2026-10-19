use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderAccount {
    pub id: Uuid,
    pub name: String,
    pub vendor: String,
    /// Non-secret operator account identifier; credentials live on deployments.
    pub account_reference: String,
    pub archived: bool,
}

impl ProviderAccount {
    pub fn validate(&self) -> Result<()> {
        for value in [&self.name, &self.vendor, &self.account_reference] {
            if value.trim().is_empty() || value.len() > 200 {
                bail!("Account identifiers must contain 1..200 characters");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRoute {
    pub id: Uuid,
    pub model_id: Uuid,
    pub inventory_id: Uuid,
    pub approved: bool,
    pub enabled: bool,
    pub archived: bool,
    pub reasoning_efforts: Option<Vec<String>>,
    pub context_limit: Option<i32>,
    pub output_limit: Option<i32>,
    pub role: String,
}

impl ModelRoute {
    pub fn validate(&self) -> Result<()> {
        super::validate_efforts(self.reasoning_efforts.as_deref())?;
        if !matches!(self.role.as_str(), "primary" | "fallback")
            || self.context_limit.is_some_and(|limit| limit <= 0)
            || self.output_limit.is_some_and(|limit| limit <= 0)
        {
            bail!("Invalid route role or token limits");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteCostRates {
    pub currency: String,
    pub billing_unit: String,
    /// Millionths of currency per billing unit; all arithmetic is integral.
    pub input_microunits: i64,
    pub output_microunits: i64,
    pub cache_read_microunits: Option<i64>,
    pub cache_write_microunits: Option<i64>,
    pub provenance: String,
}

impl RouteCostRates {
    pub fn validate(&self) -> Result<()> {
        if self.currency.len() != 3
            || !self.currency.bytes().all(|byte| byte.is_ascii_uppercase())
            || !matches!(
                self.billing_unit.as_str(),
                "million_tokens" | "request" | "image" | "audio_second"
            )
            || (self.billing_unit != "million_tokens"
                && (self.output_microunits != 0
                    || self.cache_read_microunits.is_some()
                    || self.cache_write_microunits.is_some()))
            || self.provenance.trim().is_empty()
            || self.provenance.len() > 1000
        {
            bail!("Invalid cost currency, billing unit or provenance");
        }
        if [
            Some(self.input_microunits),
            Some(self.output_microunits),
            self.cache_read_microunits,
            self.cache_write_microunits,
        ]
        .into_iter()
        .flatten()
        .any(|rate| rate < 0)
        {
            bail!("Cost rates cannot be negative");
        }
        Ok(())
    }

    /// Procurement calculation from provider usage is an estimate until an
    /// invoice is reconciled; it is never silently called a settled charge.
    pub fn usage_cost_usd(&self, usage: &inference_providers::TokenUsage) -> Option<i64> {
        if self.currency != "USD"
            || self.billing_unit != "million_tokens"
            || self.validate().is_err()
            || usage.prompt_tokens < 0
            || usage.completion_tokens < 0
        {
            return None;
        }
        // Customer billing helpers clamp malformed details. Procurement must
        // retain uncertainty instead of estimating from silently repaired data.
        let detail = |key: &str| -> Option<i64> {
            match usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.get(key))
            {
                None => Some(0),
                Some(value) => value
                    .as_i64()
                    .filter(|tokens| (0..=i64::from(usage.prompt_tokens)).contains(tokens)),
            }
        };
        if detail("cached_tokens")?
            + detail("cache_creation_tokens")?.max(detail("cache_write_tokens")?)
            > i64::from(usage.prompt_tokens)
        {
            return None;
        }
        let cached = i128::from(usage.cached_tokens());
        let written = i128::from(
            usage
                .cache_creation_tokens()
                .max(usage.cache_write_tokens()),
        );
        let uncached = i128::from(usage.prompt_tokens)
            .checked_sub(cached)?
            .checked_sub(written)?;
        if uncached < 0 {
            return None;
        }
        let read_rate = if cached > 0 {
            self.cache_read_microunits?
        } else {
            0
        };
        let write_rate = if written > 0 {
            self.cache_write_microunits?
        } else {
            0
        };
        let numerator = uncached * i128::from(self.input_microunits)
            + cached * i128::from(read_rate)
            + written * i128::from(write_rate)
            + i128::from(usage.completion_tokens) * i128::from(self.output_microunits);
        i64::try_from((numerator + 999_999) / 1_000_000).ok()
    }

    pub fn quantity_cost_usd(&self, quantity: &BillableQuantity) -> Option<i64> {
        if self.currency != "USD"
            || self.billing_unit == "million_tokens"
            || self.validate().is_err()
            || quantity.unit != self.billing_unit
            || quantity.microunits < 0
        {
            return None;
        }
        let numerator = i128::from(self.input_microunits) * i128::from(quantity.microunits);
        i64::try_from((numerator + 999_999) / 1_000_000).ok()
    }

    pub fn estimate_request(
        &self,
        input: Option<u32>,
        output: Option<u64>,
        quantity: Option<&BillableQuantity>,
    ) -> Option<i64> {
        match self.billing_unit.as_str() {
            "million_tokens" => self.estimate_usd(u64::from(input?), output?),
            "request" => self.quantity_cost_usd(&BillableQuantity::whole("request", 1)?),
            _ => self.quantity_cost_usd(quantity?),
        }
    }

    /// Conservative uncached upper estimate. Unknown currencies are not zero.
    pub fn estimate_usd(&self, input_tokens: u64, output_tokens: u64) -> Option<i64> {
        if self.currency != "USD"
            || self.billing_unit != "million_tokens"
            || self.validate().is_err()
        {
            return None;
        }
        let input_rate = self
            .input_microunits
            .max(self.cache_write_microunits.unwrap_or(0));
        let numerator = u128::from(input_tokens)
            .checked_mul(input_rate as u128)?
            .checked_add(u128::from(output_tokens).checked_mul(self.output_microunits as u128)?)?;
        i64::try_from(numerator.div_ceil(1_000_000)).ok()
    }
}

/// Fixed-point vendor units (one unit = 1,000,000 microunits).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BillableQuantity {
    pub unit: String,
    pub microunits: i64,
}

impl BillableQuantity {
    pub fn whole(unit: &str, count: u64) -> Option<Self> {
        Some(Self {
            unit: unit.into(),
            microunits: i64::try_from(count.checked_mul(1_000_000)?).ok()?,
        })
    }
    pub fn audio_seconds(seconds: f64) -> Option<Self> {
        let scaled = (seconds * 1_000_000.0).ceil();
        if !scaled.is_finite() || seconds < 0.0 || scaled >= i64::MAX as f64 {
            return None;
        }
        Some(Self {
            unit: "audio_second".into(),
            microunits: scaled as i64,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouteRuntime {
    pub account_id: Option<Uuid>,
    pub deployment_revision: Option<i64>,
    pub route_id: Uuid,
    pub deployment_id: Uuid,
    pub quota_group: String,
    pub revision: i64,
    pub context_limit: Option<i32>,
    pub output_limit: Option<i32>,
    pub role: String,
    pub cost_profile_id: Option<Uuid>,
    pub cost_rates: Option<RouteCostRates>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceAttempt {
    #[serde(default)]
    pub billable_quantity: Option<BillableQuantity>,
    pub request_id: Uuid,
    pub accepted_request_id: Option<Uuid>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub usage: Option<inference_providers::TokenUsage>,
    pub attempt_index: i32,
    pub route: RouteRuntime,
    pub effective_tee: super::TeeRequirement,
    pub reasoning_effort: Option<String>,
    pub outcome: String,
    pub fallback_reason: Option<String>,
    pub estimated_cost_microusd: Option<i64>,
    pub latency_ms: Option<i64>,
}

#[async_trait::async_trait]
pub trait AttemptRepository: Send + Sync {
    async fn claim_quota(&self, group: &str) -> Result<bool>;
    async fn record_health(
        &self,
        group: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        cooldown_ms: Option<u64>,
    ) -> Result<()>;
    /// Persist before dispatch. A failed insert must prevent the attempt.
    async fn start(&self, attempt: &InferenceAttempt) -> Result<()>;
    async fn finish(&self, attempt: &InferenceAttempt) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quantity_costs_preserve_units_rounding_and_unknowns() {
        let mut rates = RouteCostRates {
            currency: "USD".into(),
            billing_unit: "request".into(),
            input_microunits: 7,
            output_microunits: 0,
            cache_read_microunits: None,
            cache_write_microunits: None,
            provenance: "contract".into(),
        };
        assert_eq!(rates.estimate_request(None, None, None), Some(7));
        assert_eq!(rates.estimate_usd(1, 1), None);
        rates.billing_unit = "image".into();
        assert_eq!(rates.estimate_request(None, None, None), None);
        assert_eq!(
            rates.quantity_cost_usd(&BillableQuantity::whole("image", 3).unwrap()),
            Some(21)
        );
        assert_eq!(
            rates.quantity_cost_usd(&BillableQuantity::whole("request", 3).unwrap()),
            None
        );
        rates.billing_unit = "audio_second".into();
        assert_eq!(
            rates.quantity_cost_usd(&BillableQuantity::audio_seconds(0.5).unwrap()),
            Some(4)
        );
        for invalid in [f64::NAN, f64::INFINITY, -1.0, -0.00000001, f64::MAX] {
            assert!(BillableQuantity::audio_seconds(invalid).is_none());
        }
        assert!(BillableQuantity::whole("image", u64::MAX).is_none());
        rates.input_microunits = i64::MAX;
        assert_eq!(
            rates.quantity_cost_usd(&BillableQuantity::audio_seconds(2.0).unwrap()),
            None
        );
        rates.cache_read_microunits = Some(0);
        assert!(rates.validate().is_err());
    }
    #[test]
    fn cost_estimates_round_up_and_preserve_unknowns() {
        let mut rates = RouteCostRates {
            currency: "USD".into(),
            billing_unit: "million_tokens".into(),
            input_microunits: 1_000_000,
            output_microunits: 2_000_000,
            cache_read_microunits: Some(100_000),
            cache_write_microunits: Some(1_250_000),
            provenance: "contract".into(),
        };
        assert_eq!(rates.estimate_usd(1, 1), Some(4));
        for invalid in [-1, 11] {
            let usage: inference_providers::TokenUsage =
                serde_json::from_value(serde_json::json!({
                    "prompt_tokens":10,"completion_tokens":1,"total_tokens":11,
                    "prompt_tokens_details":{"cached_tokens":invalid}
                }))
                .unwrap();
            assert_eq!(rates.usage_cost_usd(&usage), None);
        }
        rates.currency = "EUR".into();
        assert_eq!(rates.estimate_usd(1, 1), None);
        rates.currency = "USD".into();
        rates.output_microunits = i64::MAX;
        assert_eq!(rates.estimate_usd(u64::MAX, u64::MAX), None);
    }
}
