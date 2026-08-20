use super::{CostBreakdown, UsageError};
use serde::{Deserialize, Serialize};

const NANO_USD_PER_USD: i128 = 1_000_000_000;
const TOKENS_PER_PRICING_UNIT: i128 = 1_000_000;

/// Versioned, exact text pricing stored in the model catalog.
///
/// Rates are decimal USD strings per one million tokens. Strings are used on
/// purpose: provider list prices must never pass through a binary float.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextPricingProfile {
    pub version: u32,
    pub currency: String,
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context_threshold: Option<i32>,
    pub tiers: TextPricingTiers,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextPricingTiers {
    pub default: TextTierPricing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex: Option<TextTierPricing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<TextTierPricing>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextTierPricing {
    pub short: TextTokenRates,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long: Option<TextTokenRates>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextTokenRates {
    pub uncached_input: String,
    pub cached_input: String,
    /// Absent when the provider publishes no price for this token class. A
    /// positive unpriced class is billed at the highest applicable input rate
    /// and raises the critical billing fallback signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<String>,
    pub output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextServiceTier {
    Default,
    Flex,
    Priority,
}

impl TextServiceTier {
    pub fn parse_request(value: Option<&str>) -> Result<Self, UsageError> {
        match value.unwrap_or("default") {
            "auto" | "default" => Ok(Self::Default),
            "flex" => Ok(Self::Flex),
            "fast" | "priority" => Ok(Self::Priority),
            other => Err(UsageError::ValidationError(format!(
                "unsupported service_tier '{other}'; expected auto, default, flex, fast, or priority"
            ))),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Flex => "flex",
            Self::Priority => "priority",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextContextBand {
    Short,
    Long,
}

impl TextContextBand {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Long => "long",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextBillingSnapshot {
    pub profile_version: u32,
    pub requested_tier: String,
    pub actual_tier: String,
    pub provider_reported_tier: Option<String>,
    pub priced_tier: String,
    pub context_band: TextContextBand,
    pub rates: TextTokenRates,
    pub rounding: TextPricingRounding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical_fallback_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextPricingRounding {
    pub mode: String,
    pub unit: String,
    pub exact_numerator: String,
    pub denominator: i64,
    pub rounded_total: i64,
}

#[derive(Debug, Clone)]
pub struct TextPricingCost {
    pub cost: CostBreakdown,
    pub snapshot: TextBillingSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedRates {
    uncached_input: i128,
    cached_input: i128,
    cache_write: Option<i128>,
    output: i128,
}

impl TextPricingProfile {
    pub fn from_json(value: serde_json::Value) -> Result<Self, UsageError> {
        let profile: Self = serde_json::from_value(value).map_err(|error| {
            UsageError::ValidationError(format!("invalid textPricing profile: {error}"))
        })?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), UsageError> {
        if self.version != 1 {
            return Err(UsageError::ValidationError(
                "textPricing.version must be 1".to_string(),
            ));
        }
        if self.currency != "USD" {
            return Err(UsageError::ValidationError(
                "textPricing.currency must be USD".to_string(),
            ));
        }
        if self.unit != "million_tokens" {
            return Err(UsageError::ValidationError(
                "textPricing.unit must be million_tokens".to_string(),
            ));
        }
        if self.long_context_threshold.is_some_and(|value| value <= 0) {
            return Err(UsageError::ValidationError(
                "textPricing.longContextThreshold must be positive".to_string(),
            ));
        }

        let tiers = [
            (TextServiceTier::Default, Some(&self.tiers.default)),
            (TextServiceTier::Flex, self.tiers.flex.as_ref()),
            (TextServiceTier::Priority, self.tiers.priority.as_ref()),
        ];
        let has_long = tiers
            .iter()
            .filter_map(|(_, tier)| *tier)
            .any(|tier| tier.long.is_some());
        if has_long && self.long_context_threshold.is_none() {
            return Err(UsageError::ValidationError(
                "textPricing.longContextThreshold is required when long rates are configured"
                    .to_string(),
            ));
        }
        if self.long_context_threshold.is_some() && self.tiers.default.long.is_none() {
            return Err(UsageError::ValidationError(
                "default textPricing tier must include long rates when longContextThreshold is configured"
                    .to_string(),
            ));
        }

        for (name, tier) in tiers
            .into_iter()
            .filter_map(|(name, tier)| tier.map(|t| (name, t)))
        {
            tier.short.parse().map_err(|error| {
                UsageError::ValidationError(format!(
                    "invalid {} short textPricing rates: {error}",
                    name.as_str()
                ))
            })?;
            if let Some(long) = &tier.long {
                long.parse().map_err(|error| {
                    UsageError::ValidationError(format!(
                        "invalid {} long textPricing rates: {error}",
                        name.as_str()
                    ))
                })?;
            }
        }
        Ok(())
    }

    pub fn supports_tier(&self, tier: TextServiceTier) -> bool {
        self.tier(tier).is_some()
    }

    pub fn context_band(&self, prompt_tokens: i32) -> TextContextBand {
        match self.long_context_threshold {
            Some(threshold) if prompt_tokens > threshold => TextContextBand::Long,
            _ => TextContextBand::Short,
        }
    }

    /// Standard-short projection for the legacy nano-USD-per-token columns.
    pub fn legacy_projection(&self) -> Result<(i64, i64, i64), UsageError> {
        self.validate()?;
        Ok((
            rate_per_token_half_up(&self.tiers.default.short.uncached_input)?,
            rate_per_token_half_up(&self.tiers.default.short.cached_input)?,
            rate_per_token_half_up(&self.tiers.default.short.output)?,
        ))
    }

    fn tier(&self, tier: TextServiceTier) -> Option<&TextTierPricing> {
        match tier {
            TextServiceTier::Default => Some(&self.tiers.default),
            TextServiceTier::Flex => self.tiers.flex.as_ref(),
            TextServiceTier::Priority => self.tiers.priority.as_ref(),
        }
    }

    fn rates(&self, tier: TextServiceTier, band: TextContextBand) -> Option<&TextTokenRates> {
        let tier = self.tier(tier)?;
        match band {
            TextContextBand::Short => Some(&tier.short),
            TextContextBand::Long => tier.long.as_ref(),
        }
    }

    fn highest_rates(&self) -> Result<TextTokenRates, UsageError> {
        // A missing tier/band or an unknown provider tier is a critical
        // fail-safe path. Scan every configured tier and band so a short-only
        // Priority rate cannot be undercut by a cheaper long-context tier.
        let candidates: Vec<&TextTokenRates> = [
            Some(&self.tiers.default.short),
            self.tiers.default.long.as_ref(),
            self.tiers.flex.as_ref().map(|tier| &tier.short),
            self.tiers.flex.as_ref().and_then(|tier| tier.long.as_ref()),
            self.tiers.priority.as_ref().map(|tier| &tier.short),
            self.tiers
                .priority
                .as_ref()
                .and_then(|tier| tier.long.as_ref()),
        ]
        .into_iter()
        .flatten()
        .collect();
        if candidates.is_empty() {
            return Err(UsageError::ValidationError(
                "textPricing has no configured rates".to_string(),
            ));
        }

        fn highest(
            candidates: &[&TextTokenRates],
            get: impl Fn(&TextTokenRates) -> &String,
        ) -> Result<String, UsageError> {
            let mut selected = get(candidates[0]).clone();
            let mut selected_value = parse_rate(&selected)?;
            for candidate in &candidates[1..] {
                let value = parse_rate(get(candidate))?;
                if value > selected_value {
                    selected = get(candidate).clone();
                    selected_value = value;
                }
            }
            Ok(selected)
        }

        Ok(TextTokenRates {
            uncached_input: highest(&candidates, |rates| &rates.uncached_input)?,
            cached_input: highest(&candidates, |rates| &rates.cached_input)?,
            cache_write: {
                let configured: Vec<&String> = candidates
                    .iter()
                    .filter_map(|rates| rates.cache_write.as_ref())
                    .collect();
                if configured.is_empty() {
                    None
                } else {
                    let mut selected = configured[0].clone();
                    let mut selected_value = parse_rate(&selected)?;
                    for candidate in &configured[1..] {
                        let value = parse_rate(candidate)?;
                        if value > selected_value {
                            selected = (*candidate).clone();
                            selected_value = value;
                        }
                    }
                    Some(selected)
                }
            },
            output: highest(&candidates, |rates| &rates.output)?,
        })
    }

    fn highest_input_rate(&self, band: TextContextBand) -> Result<String, UsageError> {
        let candidates: Vec<&TextTokenRates> = [
            self.rates(TextServiceTier::Default, band),
            self.rates(TextServiceTier::Flex, band),
            self.rates(TextServiceTier::Priority, band),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut selected: Option<(String, i128)> = None;
        for rates in candidates {
            for value in [
                Some(&rates.uncached_input),
                Some(&rates.cached_input),
                rates.cache_write.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                let parsed = parse_rate(value)?;
                if selected
                    .as_ref()
                    .is_none_or(|(_, selected_value)| parsed > *selected_value)
                {
                    selected = Some((value.clone(), parsed));
                }
            }
        }
        selected.map(|(value, _)| value).ok_or_else(|| {
            UsageError::ValidationError(format!("textPricing has no {} input rates", band.as_str()))
        })
    }
}

impl TextTokenRates {
    fn parse(&self) -> Result<ParsedRates, UsageError> {
        Ok(ParsedRates {
            uncached_input: parse_rate(&self.uncached_input)?,
            cached_input: parse_rate(&self.cached_input)?,
            cache_write: self.cache_write.as_deref().map(parse_rate).transpose()?,
            output: parse_rate(&self.output)?,
        })
    }
}

pub fn compute_profiled_text_cost(
    input_tokens: i32,
    output_tokens: i32,
    cache_read_tokens: i32,
    cache_write_tokens: i32,
    requested_tier: TextServiceTier,
    provider_reported_tier: Option<&str>,
    profile: &TextPricingProfile,
) -> Result<TextPricingCost, UsageError> {
    if input_tokens < 0 || output_tokens < 0 || cache_read_tokens < 0 || cache_write_tokens < 0 {
        return Err(UsageError::ValidationError(
            "token counts must be non-negative".to_string(),
        ));
    }
    profile.validate()?;

    let band = profile.context_band(input_tokens);
    let (actual_tier, mut rates, priced_tier, mut critical_fallback_reason) =
        match provider_reported_tier {
            None => match profile.rates(requested_tier, band) {
                Some(rates) => (
                    requested_tier,
                    rates.clone(),
                    requested_tier.as_str().to_string(),
                    None,
                ),
                None => (
                    requested_tier,
                    profile.highest_rates()?,
                    "highest_configured".to_string(),
                    Some("forwarded_tier_context_band_not_priced".to_string()),
                ),
            },
            Some(value) => match parse_provider_tier(value) {
                Ok(actual) if profile.rates(actual, band).is_some() => (
                    actual,
                    profile.rates(actual, band).expect("checked above").clone(),
                    actual.as_str().to_string(),
                    None,
                ),
                Ok(actual) => (
                    actual,
                    profile.highest_rates()?,
                    "highest_configured".to_string(),
                    Some("returned_tier_not_priced".to_string()),
                ),
                Err(_) => (
                    requested_tier,
                    profile.highest_rates()?,
                    "highest_configured".to_string(),
                    Some("unknown_provider_tier".to_string()),
                ),
            },
        };

    let cache_read = cache_read_tokens.min(input_tokens).max(0) as i128;
    let cache_write = cache_write_tokens
        .min(input_tokens.saturating_sub(cache_read as i32))
        .max(0) as i128;
    if rates.cache_write.is_none() {
        rates.cache_write = Some(profile.highest_input_rate(band)?);
        if cache_write > 0 && critical_fallback_reason.is_none() {
            critical_fallback_reason = Some("unpriced_positive_cache_write".to_string());
        }
    }
    let uncached_input = i128::from(input_tokens) - cache_read - cache_write;
    let output = i128::from(output_tokens);
    let parsed = rates.parse()?;

    let input_numerator = uncached_input
        .checked_mul(parsed.uncached_input)
        .and_then(|value| {
            cache_read
                .checked_mul(parsed.cached_input)
                .and_then(|cached| value.checked_add(cached))
        })
        .and_then(|value| {
            cache_write
                .checked_mul(
                    parsed
                        .cache_write
                        .expect("cache-write fallback filled above"),
                )
                .and_then(|written| value.checked_add(written))
        })
        .ok_or_else(|| UsageError::CostCalculationOverflow("profiled input cost".to_string()))?;
    let output_numerator = output
        .checked_mul(parsed.output)
        .ok_or_else(|| UsageError::CostCalculationOverflow("profiled output cost".to_string()))?;
    let total_numerator = input_numerator
        .checked_add(output_numerator)
        .ok_or_else(|| UsageError::CostCalculationOverflow("profiled total cost".to_string()))?;
    let total_cost = round_half_up(total_numerator, TOKENS_PER_PRICING_UNIT)?;
    let (input_cost, output_cost) = apportion_rounded_total(
        input_numerator,
        output_numerator,
        total_cost,
        TOKENS_PER_PRICING_UNIT,
    )?;

    Ok(TextPricingCost {
        cost: CostBreakdown {
            input_cost,
            output_cost,
            total_cost,
        },
        snapshot: TextBillingSnapshot {
            profile_version: profile.version,
            requested_tier: requested_tier.as_str().to_string(),
            actual_tier: actual_tier.as_str().to_string(),
            provider_reported_tier: provider_reported_tier.map(str::to_string),
            priced_tier,
            context_band: band,
            rates,
            rounding: TextPricingRounding {
                mode: "half_up".to_string(),
                unit: "nano_usd".to_string(),
                exact_numerator: total_numerator.to_string(),
                denominator: TOKENS_PER_PRICING_UNIT as i64,
                rounded_total: total_cost,
            },
            critical_fallback_reason,
        },
    })
}

fn parse_provider_tier(value: &str) -> Result<TextServiceTier, ()> {
    match value {
        "default" => Ok(TextServiceTier::Default),
        "flex" => Ok(TextServiceTier::Flex),
        "fast" | "priority" => Ok(TextServiceTier::Priority),
        _ => Err(()),
    }
}

fn parse_rate(value: &str) -> Result<i128, UsageError> {
    if value.is_empty() {
        return Err(UsageError::ValidationError(
            "rate must be a non-empty decimal string".to_string(),
        ));
    }
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|digits| {
            digits.is_empty()
                || digits.len() > 9
                || !digits.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(UsageError::ValidationError(format!(
            "'{value}' is not a non-negative decimal with at most 9 fractional digits"
        )));
    }
    let whole = whole.parse::<i128>().map_err(|_| {
        UsageError::ValidationError(format!("rate '{value}' is outside the supported range"))
    })?;
    let mut nanos = whole.checked_mul(NANO_USD_PER_USD).ok_or_else(|| {
        UsageError::ValidationError(format!("rate '{value}' is outside the supported range"))
    })?;
    if let Some(fraction) = fraction {
        let fraction_value = fraction
            .parse::<i128>()
            .map_err(|_| UsageError::ValidationError(format!("invalid decimal rate '{value}'")))?;
        let scale = 10_i128.pow((9 - fraction.len()) as u32);
        nanos = nanos
            .checked_add(fraction_value.checked_mul(scale).ok_or_else(|| {
                UsageError::ValidationError(format!(
                    "rate '{value}' is outside the supported range"
                ))
            })?)
            .ok_or_else(|| {
                UsageError::ValidationError(format!(
                    "rate '{value}' is outside the supported range"
                ))
            })?;
    }
    Ok(nanos)
}

fn rate_per_token_half_up(value: &str) -> Result<i64, UsageError> {
    round_half_up(parse_rate(value)?, TOKENS_PER_PRICING_UNIT)
}

fn round_half_up(numerator: i128, denominator: i128) -> Result<i64, UsageError> {
    let rounded = numerator
        .checked_add(denominator / 2)
        .and_then(|value| value.checked_div(denominator))
        .ok_or_else(|| UsageError::CostCalculationOverflow("rounding profiled cost".to_string()))?;
    i64::try_from(rounded)
        .map_err(|_| UsageError::CostCalculationOverflow("profiled cost exceeds i64".to_string()))
}

fn apportion_rounded_total(
    input_numerator: i128,
    output_numerator: i128,
    rounded_total: i64,
    denominator: i128,
) -> Result<(i64, i64), UsageError> {
    let mut input = input_numerator / denominator;
    let mut output = output_numerator / denominator;
    let mut residual = i128::from(rounded_total) - input - output;
    let input_remainder = input_numerator % denominator;
    let output_remainder = output_numerator % denominator;

    if residual > 0 {
        if input_remainder >= output_remainder {
            input += 1;
        } else {
            output += 1;
        }
        residual -= 1;
    }
    if residual > 0 {
        if input_remainder >= output_remainder {
            output += residual;
        } else {
            input += residual;
        }
    }

    Ok((
        i64::try_from(input).map_err(|_| {
            UsageError::CostCalculationOverflow("profiled input cost exceeds i64".to_string())
        })?,
        i64::try_from(output).map_err(|_| {
            UsageError::CostCalculationOverflow("profiled output cost exceeds i64".to_string())
        })?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> TextPricingProfile {
        TextPricingProfile {
            version: 1,
            currency: "USD".to_string(),
            unit: "million_tokens".to_string(),
            long_context_threshold: Some(272_000),
            tiers: TextPricingTiers {
                default: TextTierPricing {
                    short: TextTokenRates {
                        uncached_input: "5.00".to_string(),
                        cached_input: "0.50".to_string(),
                        cache_write: Some("6.25".to_string()),
                        output: "30.00".to_string(),
                    },
                    long: Some(TextTokenRates {
                        uncached_input: "10.00".to_string(),
                        cached_input: "1.00".to_string(),
                        cache_write: Some("12.50".to_string()),
                        output: "45.00".to_string(),
                    }),
                },
                flex: Some(TextTierPricing {
                    short: TextTokenRates {
                        uncached_input: "2.50".to_string(),
                        cached_input: "0.25".to_string(),
                        cache_write: Some("3.125".to_string()),
                        output: "15.00".to_string(),
                    },
                    long: Some(TextTokenRates {
                        uncached_input: "5.00".to_string(),
                        cached_input: "0.50".to_string(),
                        cache_write: Some("6.25".to_string()),
                        output: "22.50".to_string(),
                    }),
                }),
                priority: Some(TextTierPricing {
                    short: TextTokenRates {
                        uncached_input: "10.00".to_string(),
                        cached_input: "1.00".to_string(),
                        cache_write: Some("12.50".to_string()),
                        output: "60.00".to_string(),
                    },
                    long: Some(TextTokenRates {
                        uncached_input: "20.00".to_string(),
                        cached_input: "2.00".to_string(),
                        cache_write: Some("25.00".to_string()),
                        output: "90.00".to_string(),
                    }),
                }),
            },
        }
    }

    #[test]
    fn normalizes_tier_aliases() {
        assert_eq!(
            TextServiceTier::parse_request(Some("auto")).unwrap(),
            TextServiceTier::Default
        );
        assert_eq!(
            TextServiceTier::parse_request(Some("fast")).unwrap(),
            TextServiceTier::Priority
        );
    }

    #[test]
    fn long_context_boundary_is_strictly_greater() {
        let profile = profile();
        assert_eq!(profile.context_band(272_000), TextContextBand::Short);
        assert_eq!(profile.context_band(272_001), TextContextBand::Long);
    }

    #[test]
    fn prices_all_token_categories_and_rounds_once() {
        let result = compute_profiled_text_cost(
            1_000,
            100,
            200,
            300,
            TextServiceTier::Default,
            Some("default"),
            &profile(),
        )
        .unwrap();
        // 500 * $5/M + 200 * $0.5/M + 300 * $6.25/M + 100 * $30/M
        assert_eq!(result.cost.total_cost, 7_475_000);
        assert_eq!(
            result.cost.input_cost + result.cost.output_cost,
            result.cost.total_cost
        );
    }

    #[test]
    fn selects_default_flex_and_priority_rates() {
        let profile = profile();
        for (tier, provider_tier, expected) in [
            (TextServiceTier::Default, "default", 5_000_000),
            (TextServiceTier::Flex, "flex", 2_500_000),
            (TextServiceTier::Priority, "priority", 10_000_000),
            (TextServiceTier::Priority, "fast", 10_000_000),
        ] {
            let result =
                compute_profiled_text_cost(1_000, 0, 0, 0, tier, Some(provider_tier), &profile)
                    .unwrap();
            assert_eq!(result.cost.total_cost, expected, "{provider_tier}");
        }
    }

    #[test]
    fn priority_request_downgraded_to_default_uses_default_price() {
        let result = compute_profiled_text_cost(
            1_000,
            0,
            0,
            0,
            TextServiceTier::Priority,
            Some("default"),
            &profile(),
        )
        .unwrap();
        assert_eq!(result.cost.total_cost, 5_000_000);
        assert_eq!(result.snapshot.requested_tier, "priority");
        assert_eq!(result.snapshot.actual_tier, "default");
        assert_eq!(result.snapshot.priced_tier, "default");
    }

    #[test]
    fn supports_fractional_per_million_rates_below_one_nano_per_token() {
        let mut profile = profile();
        profile.long_context_threshold = None;
        profile.tiers.default.long = None;
        profile.tiers.flex = None;
        profile.tiers.priority = None;
        profile.tiers.default.short = TextTokenRates {
            uncached_input: "0.0005".to_string(),
            cached_input: "0.0005".to_string(),
            cache_write: Some("0.0005".to_string()),
            output: "0.0005".to_string(),
        };
        let result =
            compute_profiled_text_cost(1, 0, 0, 0, TextServiceTier::Default, None, &profile)
                .unwrap();
        assert_eq!(result.cost.total_cost, 1);
    }

    #[test]
    fn unknown_provider_tier_uses_highest_configured_rates() {
        let result = compute_profiled_text_cost(
            1_000,
            0,
            0,
            0,
            TextServiceTier::Default,
            Some("future"),
            &profile(),
        )
        .unwrap();
        assert_eq!(result.cost.total_cost, 20_000_000);
        assert_eq!(result.snapshot.priced_tier, "highest_configured");
        assert_eq!(
            result.snapshot.critical_fallback_reason.as_deref(),
            Some("unknown_provider_tier")
        );
    }

    #[test]
    fn unpriced_positive_cache_write_uses_highest_applicable_input_rate() {
        let mut profile = profile();
        profile.tiers.default.short.cache_write = None;
        profile.tiers.default.long.as_mut().unwrap().cache_write = None;
        for tier in [&mut profile.tiers.flex, &mut profile.tiers.priority]
            .into_iter()
            .flatten()
        {
            tier.short.cache_write = None;
            if let Some(long) = &mut tier.long {
                long.cache_write = None;
            }
        }

        let result = compute_profiled_text_cost(
            1_000,
            0,
            0,
            100,
            TextServiceTier::Default,
            Some("default"),
            &profile,
        )
        .unwrap();
        // 900 default uncached tokens at $5/M plus the unexpected cache write
        // at the highest configured short-context input rate ($10/M).
        assert_eq!(result.cost.total_cost, 5_500_000);
        assert_eq!(result.snapshot.rates.cache_write.as_deref(), Some("10.00"));
        assert_eq!(
            result.snapshot.critical_fallback_reason.as_deref(),
            Some("unpriced_positive_cache_write")
        );
    }

    #[test]
    fn allows_tier_that_is_only_advertised_for_short_context() {
        let mut profile = profile();
        profile.tiers.flex.as_mut().unwrap().long = None;
        assert!(profile.validate().is_ok());
        let result = compute_profiled_text_cost(
            272_001,
            0,
            0,
            0,
            TextServiceTier::Flex,
            Some("flex"),
            &profile,
        )
        .unwrap();
        assert_eq!(result.snapshot.priced_tier, "highest_configured");
        assert_eq!(
            result.snapshot.critical_fallback_reason.as_deref(),
            Some("returned_tier_not_priced")
        );
    }

    #[test]
    fn short_only_priority_long_fallback_never_uses_cheaper_long_rates() {
        let mut profile = profile();
        profile.tiers.priority.as_mut().unwrap().long = None;
        let result = compute_profiled_text_cost(
            272_001,
            1,
            0,
            0,
            TextServiceTier::Priority,
            Some("priority"),
            &profile,
        )
        .unwrap();

        assert_eq!(result.snapshot.rates.uncached_input, "10.00");
        assert_eq!(result.snapshot.rates.output, "60.00");
        assert_eq!(result.snapshot.priced_tier, "highest_configured");
        assert_eq!(
            result.snapshot.critical_fallback_reason.as_deref(),
            Some("returned_tier_not_priced")
        );
    }

    #[test]
    fn tier_removed_in_flight_falls_back_instead_of_dropping_usage() {
        let mut profile = profile();
        profile.tiers.priority = None;
        let result =
            compute_profiled_text_cost(1_000, 0, 0, 0, TextServiceTier::Priority, None, &profile)
                .unwrap();

        assert_eq!(result.snapshot.actual_tier, "priority");
        assert_eq!(result.snapshot.priced_tier, "highest_configured");
        assert_eq!(
            result.snapshot.critical_fallback_reason.as_deref(),
            Some("forwarded_tier_context_band_not_priced")
        );
    }

    #[test]
    fn provider_auto_is_unknown_and_fails_safe() {
        let result = compute_profiled_text_cost(
            1_000,
            0,
            0,
            0,
            TextServiceTier::Default,
            Some("auto"),
            &profile(),
        )
        .unwrap();
        assert_eq!(result.snapshot.priced_tier, "highest_configured");
        assert_eq!(
            result.snapshot.critical_fallback_reason.as_deref(),
            Some("unknown_provider_tier")
        );
    }

    #[test]
    fn rejects_costs_that_overflow_the_nano_usd_ledger() {
        let mut profile = profile();
        profile.long_context_threshold = None;
        profile.tiers.flex = None;
        profile.tiers.priority = None;
        profile.tiers.default.long = None;
        profile.tiers.default.short = TextTokenRates {
            uncached_input: "10000000000000000".to_string(),
            cached_input: "10000000000000000".to_string(),
            cache_write: Some("10000000000000000".to_string()),
            output: "10000000000000000".to_string(),
        };

        assert!(matches!(
            compute_profiled_text_cost(
                i32::MAX,
                i32::MAX,
                0,
                0,
                TextServiceTier::Default,
                None,
                &profile,
            ),
            Err(UsageError::CostCalculationOverflow(_))
        ));
    }

    #[test]
    fn projection_rounds_to_legacy_nano_usd_per_token() {
        let mut profile = profile();
        profile.tiers.default.short.uncached_input = "0.0005".to_string();
        let (input, cached, output) = profile.legacy_projection().unwrap();
        assert_eq!(input, 1);
        assert_eq!(cached, 500);
        assert_eq!(output, 30_000);
    }
}
