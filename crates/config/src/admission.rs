//! Local estimation policy. Values affect admission only, never provider requests.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EstimatePolicy {
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

impl Default for EstimatePolicy {
    fn default() -> Self {
        Self {
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdmissionConfig {
    pub version: u32,
    pub organization_max_unsettled: i32,
    pub key_max_unsettled: i32,
    pub default: EstimatePolicy,
    pub kinds: BTreeMap<String, EstimatePolicy>,
    pub models: BTreeMap<String, EstimatePolicy>,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            version: 1,
            organization_max_unsettled: 64,
            key_max_unsettled: 16,
            default: EstimatePolicy::default(),
            kinds: BTreeMap::new(),
            models: BTreeMap::new(),
        }
    }
}

impl AdmissionConfig {
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let config: Self = match raw {
            Some(raw) => serde_json::from_str(raw).map_err(|_| {
                "USAGE_ADMISSION_CONFIG must be a valid admission policy".to_string()
            })?,
            None => Self::default(),
        };
        if config.version == 0
            || config.organization_max_unsettled <= 0
            || config.key_max_unsettled <= 0
        {
            return Err("Admission version and unsettled limits must be positive".into());
        }
        for policy in std::iter::once(&config.default)
            .chain(config.kinds.values())
            .chain(config.models.values())
        {
            if policy.multiplier_basis_points == 0
                || policy.minimum_nano_usd <= 0
                || policy.bytes_per_token == 0
                || policy.default_output_tokens <= 0
                || policy.image_input_tokens <= 0
                || policy.audio_input_tokens <= 0
                || policy.video_input_tokens <= 0
                || policy.unknown_input_tokens <= 0
                || policy.audio_bytes_per_second == 0
                || policy.default_audio_seconds <= 0
                || policy.decision_output_tokens <= 0
            {
                return Err("Admission estimator parameters must be positive".into());
            }
        }
        Ok(config)
    }

    pub fn policy(&self, model: &str, kind: &str) -> &EstimatePolicy {
        self.models
            .get(model)
            .or_else(|| self.kinds.get(kind))
            .unwrap_or(&self.default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_validation_and_override_precedence() {
        for invalid in [
            r#"{"key_max_unsettled":0}"#,
            r#"{"default":{"bytes_per_token":0}}"#,
            r#"{"unknown":1}"#,
        ] {
            assert!(AdmissionConfig::parse(Some(invalid)).is_err());
        }
        let config = AdmissionConfig::parse(Some(r#"{"kinds":{"audio_transcription":{"multiplier_basis_points":30000}},"models":{"speech":{"multiplier_basis_points":40000}}}"#)).unwrap();
        assert_eq!(
            config
                .policy("speech", "audio_transcription")
                .multiplier_basis_points,
            40000
        );
        assert_eq!(
            config
                .policy("other", "audio_transcription")
                .multiplier_basis_points,
            30000
        );
        assert_eq!(
            config
                .policy("text", "chat_completion")
                .multiplier_basis_points,
            20000
        );
    }
}
