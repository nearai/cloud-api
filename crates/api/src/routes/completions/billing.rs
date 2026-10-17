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
    Uuid::parse_str(&api_key.api_key.id.0).map_err(|e| {
        tracing::error!(error = %e, "Invalid API key ID for usage tracking");
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

/// Per-endpoint log messages for [`cap_reported_tokens`].
pub(super) struct TokenAnomalyMessages {
    pub overflow: &'static str,
    pub zero: &'static str,
    pub anomaly: &'static str,
}

/// Cap a provider-reported token count at [`MAX_REASONABLE_TOKENS`] and flag
/// a zero count, logging and emitting the provider-anomaly metrics tagged
/// with `model`. Returns the count to bill.
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
    }

    if token_count == 0 {
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
    fn zero_is_billed_as_zero_and_flagged() {
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
    fn negative_counts_pass_through_unflagged() {
        assert_eq!(cap(-5), (-5, vec![]));
    }
}
