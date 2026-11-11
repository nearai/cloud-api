//! Admission controls use the existing operator-only admin settings registry.

pub(in crate::usage) const ESTIMATOR_VERSION: u32 = 2;
use crate::admin_settings::SettingKey;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionMode {
    #[default]
    Enforce,
    Audit,
    Off,
}

impl AdmissionMode {
    /// Both values come from trusted operators, never from public inference requests.
    pub fn combined(self, requested: Self) -> Self {
        match (self, requested) {
            (Self::Off, _) | (_, Self::Off) => Self::Off,
            (Self::Audit, _) | (_, Self::Audit) => Self::Audit,
            _ => Self::Enforce,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdmissionPolicy {
    pub mode: AdmissionMode,
    pub organization_max_unsettled: i32,
    pub key_max_unsettled: i32,
    pub reservation_ttl_seconds: i32,
    pub unknown_outcome_ttl_seconds: i32,
}

impl Default for AdmissionPolicy {
    fn default() -> Self {
        Self {
            mode: AdmissionMode::Enforce,
            organization_max_unsettled: 64,
            key_max_unsettled: 16,
            reservation_ttl_seconds: 7200,
            unknown_outcome_ttl_seconds: 300,
        }
    }
}

impl AdmissionPolicy {
    fn validate(&self) -> Result<(), String> {
        if self.organization_max_unsettled <= 0 || self.key_max_unsettled <= 0 {
            return Err("unsettled limits must be positive".into());
        }
        if !(1..=86400).contains(&self.reservation_ttl_seconds)
            || !(1..=self.reservation_ttl_seconds).contains(&self.unknown_outcome_ttl_seconds)
        {
            return Err(
                "hold TTL must be 1..86400 seconds and unknown-outcome TTL must not exceed it"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdmissionSettings {
    pub mode: AdmissionMode,
    pub defaults: AdmissionPolicy,
    /// Complete overrides, held in operator settings rather than owner-editable org settings.
    pub organizations: BTreeMap<Uuid, AdmissionPolicy>,
}

impl AdmissionSettings {
    pub fn for_organization(&self, id: Uuid) -> AdmissionPolicy {
        self.organizations
            .get(&id)
            .unwrap_or(&self.defaults)
            .clone()
    }
}

pub struct UsageAdmissionKey;
impl SettingKey for UsageAdmissionKey {
    const NAME: &'static str = "usage_admission";
    const FIELDS: &'static [&'static str] = &["mode", "defaults", "organizations"];
    type Value = AdmissionSettings;
    type Published = AdmissionSettings;
    fn default_value() -> Self::Value {
        Self::Value::default()
    }
    fn parse(value: &serde_json::Value, strict: bool) -> Result<Self::Value, String> {
        let mut value = value.clone();
        if !strict {
            // Preserve known fields when rolling back a binary after an additive setting change.
            if let Some(object) = value.as_object_mut() {
                object.retain(|key, _| Self::FIELDS.contains(&key.as_str()));
                let fields = [
                    "mode",
                    "organization_max_unsettled",
                    "key_max_unsettled",
                    "reservation_ttl_seconds",
                    "unknown_outcome_ttl_seconds",
                ];
                if let Some(defaults) = object.get_mut("defaults").and_then(|v| v.as_object_mut()) {
                    defaults.retain(|key, _| fields.contains(&key.as_str()));
                }
                if let Some(organizations) = object
                    .get_mut("organizations")
                    .and_then(|v| v.as_object_mut())
                {
                    for policy in organizations.values_mut().filter_map(|v| v.as_object_mut()) {
                        policy.retain(|key, _| fields.contains(&key.as_str()));
                    }
                }
            }
        }
        let settings: Self::Value = serde_json::from_value(value).map_err(|e| e.to_string())?;
        settings.defaults.validate()?;
        for policy in settings.organizations.values() {
            policy.validate()?;
        }
        Ok(settings)
    }
    fn publish(value: &Self::Value, _version: u64) -> Self::Published {
        value.clone()
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn policy_validates_modes_limits_ttls_and_org_overrides() {
        let id = Uuid::new_v4();
        let settings = UsageAdmissionKey::parse(&json!({"defaults":{"mode":"audit"},"organizations":{id.to_string():{"organization_max_unsettled":512,"key_max_unsettled":128}}}), true).unwrap();
        assert_eq!(
            settings.for_organization(Uuid::new_v4()).mode,
            AdmissionMode::Audit
        );
        assert_eq!(
            settings.for_organization(id).organization_max_unsettled,
            512
        );
        for invalid in [
            json!({"mode":"typo"}),
            json!({"key_max_unsettled":0}),
            json!({"reservation_ttl_seconds":1}),
            json!({"unknown_outcome_ttl_seconds":0}),
            json!({"unexpected":1}),
        ] {
            assert!(UsageAdmissionKey::parse(&json!({"defaults":invalid}), true).is_err());
        }
        let stopped = UsageAdmissionKey::parse(
            &json!({"mode":"off","organizations":{id.to_string():{"mode":"enforce"}}}),
            true,
        )
        .unwrap();
        assert_eq!(stopped.for_organization(id).mode, AdmissionMode::Off);
        assert_eq!(
            AdmissionMode::Enforce.combined(AdmissionMode::Audit),
            AdmissionMode::Audit
        );
        assert_eq!(
            AdmissionMode::Off.combined(AdmissionMode::Enforce),
            AdmissionMode::Off
        );
        assert!(UsageAdmissionKey::parse(
            &json!({"defaults":{"mode":"audit","future":true},"future":1}),
            false
        )
        .is_ok());
    }
}
