//! The built-in setting keys: `placement` and `attested_3p`. Each is a
//! [`SettingKey`] strategy; the registry knows nothing about these types.

use inference_providers::{DisabledSources, ProviderSource};
use placement::Tuning;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::registry::SettingKey;
use super::AdminSettingsError;

/// The live-tunable placement knobs, in API units. Integer knobs are signed
/// so a negative value reaches the range check instead of a parse error.
/// `PlacementTuning`'s field names (a test keeps this in step with the struct).
const PLACEMENT_FIELDS: &[&str] = &[
    "affinity_abs_slack",
    "affinity_eps",
    "kv_max",
    "lane_load_tokens",
    "pin_hold_factor",
    "pin_ttl_ms",
    "enabled",
];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlacementTuning {
    /// Absolute slack of the affinity bound, in score units (0.0 - 4.0).
    pub affinity_abs_slack: f64,
    /// Relative tolerance of the affinity bound (0.0 - 2.0).
    pub affinity_eps: f64,
    /// KV usage at or above which a replica is excluded (0.5 - 1.0).
    pub kv_max: f64,
    /// Load at or above which a replica is a heavy-lane member
    /// (4000 - 1000000).
    pub lane_load_tokens: i64,
    /// Scale on the prompt's cold-prefill cost in the pin-hold test
    /// (0.5 - 2.0).
    pub pin_hold_factor: f64,
    /// How long a follow pin stays valid, in ms (60000 - 3600000).
    pub pin_ttl_ms: i64,
    /// Default true. False routes every request through legacy routing
    /// (`LegacyReason::Disabled`). A PATCH applies immediately on the instance
    /// that receives it and on other instances at the next reload
    /// ([`RELOAD_INTERVAL`]). For an instant fleet-wide stop (affects every
    /// environment sharing the Valkey) use the Valkey key
    /// `routed:_placement_off`. Set it to false only after every instance runs
    /// a build that knows this field (`deny_unknown_fields`).
    pub enabled: bool,
}

impl Default for PlacementTuning {
    fn default() -> Self {
        Tuning::default().into()
    }
}

impl From<Tuning> for PlacementTuning {
    fn from(t: Tuning) -> Self {
        Self {
            affinity_abs_slack: t.affinity_abs_slack,
            affinity_eps: t.affinity_eps,
            kv_max: t.kv_max,
            lane_load_tokens: t.lane_load_tokens as i64,
            pin_hold_factor: t.pin_hold_factor,
            pin_ttl_ms: t.pin_ttl_ms as i64,
            enabled: t.enabled,
        }
    }
}

impl PlacementTuning {
    /// The range checks. NaN fails them (every comparison with NaN is false).
    pub fn validate(&self) -> Result<(), AdminSettingsError> {
        fn float(name: &str, v: f64, lo: f64, hi: f64) -> Result<(), AdminSettingsError> {
            if v >= lo && v <= hi {
                Ok(())
            } else {
                Err(AdminSettingsError::Invalid(format!(
                    "{name} must be between {lo} and {hi}"
                )))
            }
        }
        fn int(name: &str, v: i64, lo: i64, hi: i64) -> Result<(), AdminSettingsError> {
            if (lo..=hi).contains(&v) {
                Ok(())
            } else {
                Err(AdminSettingsError::Invalid(format!(
                    "{name} must be between {lo} and {hi}"
                )))
            }
        }
        float("affinity_abs_slack", self.affinity_abs_slack, 0.0, 4.0)?;
        float("affinity_eps", self.affinity_eps, 0.0, 2.0)?;
        float("kv_max", self.kv_max, 0.5, 1.0)?;
        int("lane_load_tokens", self.lane_load_tokens, 4_000, 1_000_000)?;
        float("pin_hold_factor", self.pin_hold_factor, 0.5, 2.0)?;
        int("pin_ttl_ms", self.pin_ttl_ms, 60_000, 3_600_000)?;
        Ok(())
    }

    /// The placement crate's tuning. Only meaningful after `validate`.
    pub fn to_tuning(self) -> Tuning {
        Tuning {
            affinity_abs_slack: self.affinity_abs_slack,
            affinity_eps: self.affinity_eps,
            kv_max: self.kv_max,
            lane_load_tokens: self.lane_load_tokens as u64,
            pin_hold_factor: self.pin_hold_factor,
            pin_ttl_ms: self.pin_ttl_ms as u64,
            enabled: self.enabled,
        }
    }
}

const ATTESTED_3P_FIELDS: &[&str] = &["disabled_sources"];

/// The attested third-party kill switch: sources listed here are skipped by
/// chat routing and attestation reports. Only attested third parties can be
/// switched off.
///
/// Propagation: a PATCH applies immediately on the instance that handled it
/// and within `RELOAD_INTERVAL` (10 min) on every other instance (there is no
/// cross-instance push). For a faster stop on other instances, redeploy. Per-org
/// `fallback_disabled` is NOT a substitute: it only excludes registered
/// fallback providers for that organization, not standalone primaries or
/// attestation reports.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Attested3pSettings {
    pub disabled_sources: DisabledSources,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Attested3pWire {
    disabled_sources: Vec<String>,
}

impl Serialize for Attested3pSettings {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Attested3pWire {
            disabled_sources: self
                .disabled_sources
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Attested3pSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = Attested3pWire::deserialize(deserializer)?;
        let mut disabled_sources = DisabledSources::default();
        for name in &wire.disabled_sources {
            match ProviderSource::parse(name) {
                Some(s @ (ProviderSource::Chutes | ProviderSource::Tinfoil)) => {
                    disabled_sources.insert(s)
                }
                _ => {
                    return Err(serde::de::Error::custom(
                        "disabled_sources accepts: chutes, tinfoil",
                    ))
                }
            }
        }
        Ok(Self { disabled_sources })
    }
}

impl Attested3pSettings {
    /// Read a STORED row leniently: source names this binary does not know
    /// (written by a newer binary) are ignored, never resetting the known ones
    /// to empty, which would silently turn the kill switch off. Unknown names
    /// are logged (names only). PATCH input goes through the strict
    /// `Deserialize` instead.
    pub(super) fn from_stored(value: &Value) -> Self {
        let mut disabled_sources = DisabledSources::default();
        let names = value.get("disabled_sources").and_then(Value::as_array);
        for name in names.into_iter().flatten().filter_map(Value::as_str) {
            match ProviderSource::parse(name) {
                Some(s @ (ProviderSource::Chutes | ProviderSource::Tinfoil)) => {
                    disabled_sources.insert(s)
                }
                _ => tracing::warn!(
                    source = %name,
                    "Stored attested_3p setting names an unknown source; ignoring it"
                ),
            }
        }
        Self { disabled_sources }
    }
}

/// The `placement` setting: the live-tunable placement knobs.
pub struct PlacementKey;

impl SettingKey for PlacementKey {
    const NAME: &'static str = "placement";
    const FIELDS: &'static [&'static str] = PLACEMENT_FIELDS;
    type Value = PlacementTuning;
    type Published = Tuning;

    fn default_value() -> PlacementTuning {
        PlacementTuning::default()
    }

    /// Strict and lenient reads are the same here: stored rows are validated
    /// exactly like PATCH input.
    fn parse(value: &Value, _strict: bool) -> Result<PlacementTuning, String> {
        let typed: PlacementTuning =
            serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        typed.validate().map_err(|e| e.to_string())?;
        Ok(typed)
    }

    fn publish(value: &PlacementTuning, _version: u64) -> Tuning {
        value.to_tuning()
    }
}

/// The `attested_3p` setting: the attested third-party kill switch.
pub struct Attested3pKey;

impl SettingKey for Attested3pKey {
    const NAME: &'static str = "attested_3p";
    const FIELDS: &'static [&'static str] = ATTESTED_3P_FIELDS;
    type Value = Attested3pSettings;
    type Published = DisabledSources;

    fn default_value() -> Attested3pSettings {
        Attested3pSettings::default()
    }

    fn parse(value: &Value, strict: bool) -> Result<Attested3pSettings, String> {
        if strict {
            serde_json::from_value(value.clone()).map_err(|e| e.to_string())
        } else {
            Ok(Attested3pSettings::from_stored(value))
        }
    }

    fn publish(value: &Attested3pSettings, _version: u64) -> DisabledSources {
        value.disabled_sources
    }
}
