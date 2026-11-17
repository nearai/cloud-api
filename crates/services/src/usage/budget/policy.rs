//! Admission controls use the existing operator-only admin settings registry.

pub(in crate::usage) const ESTIMATOR_VERSION: u32 = 2;
use crate::admin_settings::SettingKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionMode {
    // The wire default permits the server policy to decide; it does not enable holds.
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
            mode: AdmissionMode::Audit,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdmissionSettings {
    /// Stops new admissions without changing organization policies or existing holds.
    pub enabled: bool,
    pub defaults: AdmissionPolicy,
}

impl Default for AdmissionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            defaults: AdmissionPolicy::default(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
pub struct AdmissionOverrides {
    pub mode: Option<AdmissionMode>,
    pub organization_max_unsettled: Option<i32>,
    pub key_max_unsettled: Option<i32>,
    pub reservation_ttl_seconds: Option<i32>,
    pub unknown_outcome_ttl_seconds: Option<i32>,
}

/// The global setting and explicit organization rows read in one database snapshot.
#[derive(Debug, Default, PartialEq)]
pub struct AdmissionSnapshot {
    pub global: AdmissionSettings,
    pub organizations: HashMap<Uuid, AdmissionOverrides>,
}

impl AdmissionSnapshot {
    pub fn from_rows(
        global: &serde_json::Value,
        organizations: HashMap<Uuid, AdmissionOverrides>,
    ) -> Result<Self, String> {
        let snapshot = Self {
            global: UsageAdmissionKey::parse(global, false)?,
            organizations,
        };
        for id in snapshot.organizations.keys() {
            snapshot.for_organization(*id).validate()?;
        }
        Ok(snapshot)
    }

    pub fn for_organization(&self, id: Uuid) -> AdmissionPolicy {
        let d = &self.global.defaults;
        let mut policy = match self.organizations.get(&id) {
            Some(o) => AdmissionPolicy {
                mode: o.mode.unwrap_or(d.mode),
                organization_max_unsettled: o
                    .organization_max_unsettled
                    .unwrap_or(d.organization_max_unsettled),
                key_max_unsettled: o.key_max_unsettled.unwrap_or(d.key_max_unsettled),
                reservation_ttl_seconds: o
                    .reservation_ttl_seconds
                    .unwrap_or(d.reservation_ttl_seconds),
                unknown_outcome_ttl_seconds: o
                    .unknown_outcome_ttl_seconds
                    .unwrap_or(d.unknown_outcome_ttl_seconds),
            },
            None => d.clone(),
        };
        if !self.global.enabled {
            policy.mode = AdmissionMode::Off;
        }
        policy
    }
}

pub struct UsageAdmissionKey;
impl SettingKey for UsageAdmissionKey {
    const NAME: &'static str = "usage_admission";
    const FIELDS: &'static [&'static str] = &["enabled", "defaults"];
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
            }
        }
        let settings: Self::Value = serde_json::from_value(value).map_err(|e| e.to_string())?;
        settings.defaults.validate()?;
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
        let global = json!({"defaults":{"key_max_unsettled":8}});
        let overrides = HashMap::from([(
            id,
            AdmissionOverrides {
                mode: Some(AdmissionMode::Enforce),
                ..Default::default()
            },
        )]);
        let mut snapshot = AdmissionSnapshot::from_rows(&global, overrides).unwrap();
        assert_eq!(snapshot.for_organization(id).mode, AdmissionMode::Enforce);
        assert_eq!(snapshot.for_organization(id).key_max_unsettled, 8);
        assert_eq!(
            snapshot.for_organization(Uuid::new_v4()).mode,
            AdmissionMode::Audit
        );
        snapshot.global.enabled = false;
        assert_eq!(snapshot.for_organization(id).mode, AdmissionMode::Off);
        snapshot.global.enabled = true;
        assert_eq!(snapshot.for_organization(id).mode, AdmissionMode::Enforce);
        assert!(AdmissionSnapshot::from_rows(
            &global,
            HashMap::from([(
                id,
                AdmissionOverrides {
                    reservation_ttl_seconds: Some(1),
                    ..Default::default()
                }
            )])
        )
        .is_err());
        for invalid in [
            json!({"mode":"typo"}),
            json!({"key_max_unsettled":0}),
            json!({"reservation_ttl_seconds":1}),
            json!({"unknown_outcome_ttl_seconds":0}),
            json!({"unexpected":1}),
        ] {
            assert!(UsageAdmissionKey::parse(&json!({"defaults":invalid}), true).is_err());
        }
        for invalid in [
            json!({"organizations":{}}),
            json!({"enabled":"false"}),
            json!({"enabled":0}),
            json!({"mode":"off"}),
        ] {
            assert!(UsageAdmissionKey::parse(&invalid, true).is_err());
        }
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
