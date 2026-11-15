//! Guards shared by the endpoints that bill provider-reported usage.

use crate::{middleware::auth::AuthenticatedApiKey, models::ErrorResponse};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Json as ResponseJson, Response},
};
use services::metrics::{consts, MetricsServiceTrait};
use uuid::Uuid;

/// Largest provider-reported token count billed as-is; above it the count is
/// treated as a provider bug and capped.
pub(super) const MAX_REASONABLE_TOKENS: i32 = 1_000_000;

/// Parse the authenticated key's id for a usage record. A malformed id fails
/// the request instead of serving it unbilled.
pub(super) fn billing_api_key_id(api_key: &AuthenticatedApiKey) -> Result<Uuid, Box<Response>> {
    Uuid::parse_str(&api_key.api_key.id.0).map_err(|_| {
        tracing::error!("Invalid API key ID for usage tracking");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                "Failed to record usage".to_string(),
                "server_error".to_string(),
            )),
        )
            .into_response()
            .into()
    })
}

/// Prefer the provider's total input count, falling back to prompt tokens
/// when a model reports only that OpenAI-compatible field.
pub(super) fn reported_input_tokens(total: Option<i32>, prompt: Option<i32>) -> i32 {
    total
        .filter(|count| *count > 0)
        .or_else(|| prompt.filter(|count| *count > 0))
        .or(total)
        .or(prompt)
        .unwrap_or(0)
}

/// Score is priced by input tokens. Prefer the explicit prompt count; when
/// only a total is available, remove a reported completion count before
/// treating it as input work.
pub(super) fn reported_score_input_tokens(
    prompt: Option<i32>,
    total: Option<i32>,
    completion: Option<i32>,
) -> i32 {
    if let Some(input) = prompt.filter(|count| *count > 0) {
        return input;
    }
    let fallback = match (total, completion) {
        (Some(total), Some(completion)) if completion >= 0 && total >= completion => {
            Some(total - completion)
        }
        (Some(total), None) => Some(total),
        _ => None,
    };
    fallback
        .filter(|count| *count > 0)
        .or(prompt)
        .or(fallback)
        .unwrap_or(0)
}

/// Audio duration is billed in whole seconds; a missing or non-finite
/// duration cannot establish the amount to charge.
pub(super) fn reported_audio_seconds(duration: Option<f64>) -> i32 {
    duration
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(|seconds| seconds.min(i32::MAX as f64).ceil() as i32)
        .unwrap_or(0)
}

/// A completed response with no billable units must not be returned as 200.
#[allow(
    clippy::result_large_err,
    reason = "Shared route guard returns a complete HTTP error response"
)]
pub(super) fn require_positive_units(units: i32) -> Result<i32, Response> {
    if units > 0 {
        Ok(units)
    } else {
        Err((
            StatusCode::BAD_GATEWAY,
            ResponseJson(ErrorResponse::new(
                "Provider did not return billable usage".to_string(),
                "server_error".to_string(),
            )),
        )
            .into_response())
    }
}

/// Per-endpoint log messages for [`cap_reported_tokens`].
pub(super) struct TokenAnomalyMessages {
    pub overflow: &'static str,
    pub zero: &'static str,
    pub anomaly: &'static str,
}

/// Cap a provider-reported token count at [`MAX_REASONABLE_TOKENS`], normalize
/// a negative count to zero, and flag non-positive counts with provider-anomaly
/// metrics tagged with `model`. Callers reject a non-positive result.
pub(super) fn cap_reported_tokens(
    metrics: &dyn MetricsServiceTrait,
    model: &str,
    organization_id: Uuid,
    mut token_count: i32,
    messages: &TokenAnomalyMessages,
) -> i32 {
    let mut token_anomaly_detected = false;

    if token_count > MAX_REASONABLE_TOKENS {
        tracing::error!(
            token_count = token_count,
            max_expected = MAX_REASONABLE_TOKENS,
            model = %model,
            organization_id = %organization_id,
            "{}",
            messages.overflow
        );
        token_anomaly_detected = true;
        let model_tag = format!("model:{}", model);
        let reason_tag = format!("reason:{}", consts::REASON_TOKEN_OVERFLOW);
        let anomaly_tags = [model_tag.as_str(), reason_tag.as_str()];
        metrics.record_count(consts::METRIC_PROVIDER_TOKEN_ANOMALIES, 1, &anomaly_tags);
        token_count = MAX_REASONABLE_TOKENS;
    } else if token_count < 0 {
        // A negative count would record a negative cost and lower the
        // organization's spend; bill nothing and flag it instead.
        tracing::error!(
            token_count = token_count,
            model = %model,
            organization_id = %organization_id,
            "Provider returned negative token count"
        );
        token_anomaly_detected = true;
        let model_tag = format!("model:{}", model);
        let reason_tag = format!("reason:{}", consts::REASON_NEGATIVE_USAGE);
        let anomaly_tags = [model_tag.as_str(), reason_tag.as_str()];
        metrics.record_count(consts::METRIC_PROVIDER_TOKEN_ANOMALIES, 1, &anomaly_tags);
        token_count = 0;
    } else if token_count == 0 {
        tracing::warn!(
            model = %model,
            organization_id = %organization_id,
            "{}",
            messages.zero
        );
        token_anomaly_detected = true;
        let model_tag = format!("model:{}", model);
        let reason_tag = format!("reason:{}", consts::REASON_MISSING_USAGE);
        let zero_tokens_tags = [model_tag.as_str(), reason_tag.as_str()];
        metrics.record_count(consts::METRIC_PROVIDER_ZERO_TOKENS, 1, &zero_tokens_tags);
    }

    if token_anomaly_detected {
        tracing::info!(
            model = %model,
            organization_id = %organization_id,
            final_token_count = token_count,
            "{}",
            messages.anomaly
        );
    }

    token_count
}

#[cfg(test)]
mod tests {
    use super::*;
    use services::metrics::capturing::{CapturingMetricsService, MetricValue};

    const MESSAGES: TokenAnomalyMessages = TokenAnomalyMessages {
        overflow: "overflow",
        zero: "zero",
        anomaly: "anomaly",
    };

    fn cap(token_count: i32) -> (i32, Vec<(String, i64, Vec<String>)>) {
        let metrics = CapturingMetricsService::new();
        let billed = cap_reported_tokens(&metrics, "m/x", Uuid::nil(), token_count, &MESSAGES);
        let recorded = metrics
            .get_metrics()
            .into_iter()
            .map(|m| match m.value {
                MetricValue::Count(value) => (m.name, value, m.tags),
                other => panic!("unexpected metric value {other:?}"),
            })
            .collect();
        (billed, recorded)
    }

    #[test]
    fn reasonable_counts_pass_through_without_metrics() {
        assert_eq!(cap(1), (1, vec![]));
        assert_eq!(cap(MAX_REASONABLE_TOKENS), (MAX_REASONABLE_TOKENS, vec![]));
    }

    #[test]
    fn overflow_is_capped_and_tagged_with_the_given_model() {
        assert_eq!(
            cap(MAX_REASONABLE_TOKENS + 1),
            (
                MAX_REASONABLE_TOKENS,
                vec![(
                    consts::METRIC_PROVIDER_TOKEN_ANOMALIES.to_string(),
                    1,
                    vec![
                        "model:m/x".to_string(),
                        format!("reason:{}", consts::REASON_TOKEN_OVERFLOW)
                    ],
                )]
            )
        );
    }

    #[test]
    fn zero_is_flagged_for_rejection() {
        assert_eq!(
            cap(0),
            (
                0,
                vec![(
                    consts::METRIC_PROVIDER_ZERO_TOKENS.to_string(),
                    1,
                    vec![
                        "model:m/x".to_string(),
                        format!("reason:{}", consts::REASON_MISSING_USAGE)
                    ],
                )]
            )
        );
    }

    #[test]
    fn negative_counts_are_flagged_for_rejection() {
        assert_eq!(
            cap(-5),
            (
                0,
                vec![(
                    consts::METRIC_PROVIDER_TOKEN_ANOMALIES.to_string(),
                    1,
                    vec![
                        "model:m/x".to_string(),
                        format!("reason:{}", consts::REASON_NEGATIVE_USAGE)
                    ],
                )]
            )
        );
    }

    #[test]
    fn reported_input_tokens_accepts_both_provider_usage_shapes() {
        assert_eq!(reported_input_tokens(Some(14), Some(10)), 14);
        assert_eq!(reported_input_tokens(None, Some(10)), 10);
        assert_eq!(reported_input_tokens(Some(0), Some(10)), 10);
        assert_eq!(reported_input_tokens(None, None), 0);
        assert_eq!(reported_input_tokens(Some(-1), None), -1);
    }

    #[test]
    fn score_uses_prompt_tokens_or_a_valid_input_only_fallback() {
        assert_eq!(reported_score_input_tokens(Some(8), Some(11), Some(3)), 8);
        assert_eq!(reported_score_input_tokens(None, Some(11), Some(3)), 8);
        assert_eq!(reported_score_input_tokens(None, Some(8), None), 8);
        assert_eq!(reported_score_input_tokens(Some(0), Some(11), Some(3)), 8);
        assert_eq!(reported_score_input_tokens(None, Some(3), Some(3)), 0);
        assert_eq!(reported_score_input_tokens(None, Some(3), Some(4)), 0);
    }

    #[test]
    fn completed_zero_usage_cannot_be_a_successful_response() {
        assert_eq!(
            require_positive_units(0).unwrap_err().status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            require_positive_units(-1).unwrap_err().status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(require_positive_units(1).unwrap(), 1);
    }

    #[test]
    fn audio_duration_requires_a_finite_positive_report() {
        assert_eq!(reported_audio_seconds(Some(0.1)), 1);
        assert_eq!(reported_audio_seconds(Some(10.1)), 11);
        assert_eq!(reported_audio_seconds(None), 0);
        assert_eq!(reported_audio_seconds(Some(0.0)), 0);
        assert_eq!(reported_audio_seconds(Some(f64::NAN)), 0);
        assert_eq!(reported_audio_seconds(Some(f64::INFINITY)), 0);
    }
}
