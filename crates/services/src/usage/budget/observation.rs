//! Bounded metrics and sampled identifiers for admission analysis. No request content.
use super::{policy::AdmissionMode, UsageFeatures};
use crate::{metrics::MetricsServiceTrait, usage::UsageError};
use std::time::Duration;
use uuid::Uuid;

pub const DECISIONS: &str = "cloud_api.admission.decisions";
pub const DURATION: &str = "cloud_api.admission.duration_seconds";

/// Sample one percent of independently generated execution IDs. Delivery retries keep
/// their original sample, while inference retries get a separate execution ID.
pub fn sampled(id: Uuid) -> bool {
    id.as_u128().is_multiple_of(100)
}

pub fn mode_label(mode: AdmissionMode) -> &'static str {
    match mode {
        AdmissionMode::Off => "off",
        AdmissionMode::Audit => "audit",
        AdmissionMode::Enforce => "enforce",
    }
}

pub fn error_reason(error: &UsageError) -> &'static str {
    match error {
        UsageError::LimitExceeded(_) => "budget_exceeded",
        UsageError::CapacityExceeded => "unsettled_limit",
        UsageError::Unauthorized(_) => "unauthorized",
        UsageError::ValidationError(_) | UsageError::CostCalculationOverflow(_) => {
            "invalid_estimate"
        }
        UsageError::ModelNotFound(_) => "model_unavailable",
        _ => "unavailable",
    }
}

pub fn duration(
    metrics: &dyn MetricsServiceTrait,
    path: &'static str,
    mode: AdmissionMode,
    stage: &'static str,
    elapsed: Duration,
) {
    let tags = [
        format!("path:{path}"),
        format!("mode:{}", mode_label(mode)),
        format!("stage:{stage}"),
    ];
    metrics.record_histogram(
        DURATION,
        elapsed.as_secs_f64(),
        &tags.each_ref().map(String::as_str),
    );
}

pub fn decision(
    metrics: &dyn MetricsServiceTrait,
    path: &'static str,
    mode: AdmissionMode,
    features: &UsageFeatures,
    outcome: &'static str,
    reason: &'static str,
) {
    let tags = [
        format!("path:{path}"),
        format!("mode:{}", mode_label(mode)),
        format!("inference_type:{}", features.inference_type.as_str()),
        format!("outcome:{outcome}"),
        format!("reason:{reason}"),
    ];
    metrics.record_count(DECISIONS, 1, &tags.each_ref().map(String::as_str));
}

pub fn input_kind(features: &UsageFeatures) -> &'static str {
    match (
        features.images > 0,
        features.audio > 0
            || features.audio_duration_seconds.is_some()
            || features.inference_type == crate::usage::InferenceType::AudioTranscription,
        features.video > 0,
        features.unknown_media > 0,
    ) {
        (false, false, false, false) => "text",
        (true, false, false, false) => "image",
        (false, true, false, false) => "audio",
        (false, false, true, false) => "video",
        (false, false, false, true) => "unknown",
        _ => "mixed",
    }
}

pub fn attempt_end(observation: Option<Uuid>, outcome: &'static str) {
    if let Some(attempt_id) = observation {
        tracing::info!(event = "usage_admission_attempt_end", %attempt_id, outcome, "Admission sample outcome");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_is_stable_and_bounded() {
        assert!(sampled(Uuid::from_u128(100)));
        assert!(!sampled(Uuid::from_u128(101)));
        assert_eq!(
            (1..=10_000)
                .filter(|id| sampled(Uuid::from_u128(*id)))
                .count(),
            100
        );
    }
}
