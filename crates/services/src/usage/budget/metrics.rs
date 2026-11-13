use super::{policy::AdmissionMode, UsageFeatures};
use crate::{metrics::MetricsServiceTrait, usage::UsageError};
use std::time::Duration;

pub const DECISIONS: &str = "cloud_api.admission.decisions";
pub const DURATION: &str = "cloud_api.admission.duration_seconds";

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
