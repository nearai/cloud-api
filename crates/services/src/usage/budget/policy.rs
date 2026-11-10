//! Versioned admission parameters, reviewed and shipped with Cloud API.

pub(in crate::usage) const ESTIMATOR_VERSION: u32 = 1;
pub(in crate::usage) const ORGANIZATION_MAX_UNSETTLED: i32 = 64;
pub(in crate::usage) const KEY_MAX_UNSETTLED: i32 = 16;
pub(in crate::usage) const RESERVATION_TTL_SECONDS: i32 = 7200;

#[derive(Debug, Clone, Copy)]
pub(super) struct EstimatePolicy {
    pub multiplier_basis_points: u32,
    pub minimum_nano_usd: i64,
    pub bytes_per_token: u32,
    pub default_output_tokens: i32,
    pub image_input_tokens: i32,
    pub audio_input_tokens: i32,
    pub video_input_tokens: i32,
    pub unknown_input_tokens: i32,
    pub audio_bytes_per_second: u32,
    pub default_audio_seconds: i32,
    pub decision_output_tokens: i32,
}

/// Local input allowances and cost scaling used by the estimator.
pub(super) const ESTIMATE_POLICY: EstimatePolicy = EstimatePolicy {
    multiplier_basis_points: 20_000,
    minimum_nano_usd: 1_000,
    bytes_per_token: 3,
    default_output_tokens: 4096,
    image_input_tokens: 2048,
    audio_input_tokens: 8192,
    video_input_tokens: 16384,
    unknown_input_tokens: 16384,
    audio_bytes_per_second: 1024,
    default_audio_seconds: 60,
    decision_output_tokens: 128,
};
