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

/// Cap a provider-reported token count at [`MAX_REASONABLE_TOKENS`], bill a
/// negative count as zero, and flag a zero count, logging and emitting the
/// provider-anomaly metrics tagged with `model`. Returns the count to bill.
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
            "Provider returned negative token count - billing zero"
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

/// Construct the metadata passed to every upstream attempt, after request validation.
pub(crate) fn budget_execution(
    state: &crate::AppState,
    key: &AuthenticatedApiKey,
    model: &services::models::ModelWithPricing,
    kind: services::usage::InferenceType,
    input_units: i64,
    output_tokens: i64,
    images: i64,
) -> Result<services::usage::budget::BudgetExecution, Box<Response>> {
    let bad_bound = || {
        Box::new(
            (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Request exceeds supported billing bounds".into(),
                    "invalid_request_error".into(),
                )),
            )
                .into_response(),
        )
    };
    if input_units < 0 || output_tokens < 0 || images < 0 {
        return Err(bad_bound());
    }
    let bounds = services::usage::budget::UsageBounds {
        inference_type: kind,
        input_units: i32::try_from(input_units).map_err(|_| bad_bound())?,
        output_tokens: i32::try_from(output_tokens).map_err(|_| bad_bound())?,
        images: i32::try_from(images).map_err(|_| bad_bound())?,
    };
    Ok(services::usage::budget::BudgetExecution::new(
        state.usage_service.clone(),
        services::usage::BillingIdentity {
            organization_id: key.organization.id.0,
            workspace_id: key.workspace.id.0,
            api_key_id: billing_api_key_id(key)?,
            model_id: model.id,
        },
        bounds,
    ))
}

pub(crate) async fn budget_id(
    budget: &services::usage::budget::BudgetExecution,
) -> Result<Uuid, Box<Response>> {
    budget
        .id()
        .await
        .map_err(|_| Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

/// Plain input tokenization cannot exceed the encoded input bytes plus model framing.
/// Token-id arrays are also covered (one or more JSON bytes per token). Reserving the
/// context framing per input covers batching, padding, and encrypted text payloads.
pub(crate) fn batch_input_bound(body: &[u8], context: i32) -> Result<i64, Box<Response>> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| Box::new(StatusCode::BAD_REQUEST.into_response()))?;
    let items = value
        .get("input")
        .and_then(|v| v.as_array())
        .map_or(1, Vec::len)
        .max(1);
    i64::try_from(body.len())
        .ok()
        .and_then(|bytes| {
            i64::try_from(items)
                .ok()
                .and_then(|items| items.checked_mul(i64::from(context.max(0))))
                .and_then(|framing| bytes.checked_add(framing))
        })
        .ok_or_else(|| Box::new(StatusCode::BAD_REQUEST.into_response()))
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
    fn negative_counts_bill_zero_and_are_flagged() {
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
}
