//! Admin-changeable runtime settings. Each setting is a JSON value under a
//! known key in one table, so adding a setting needs neither a migration nor
//! an endpoint: a typed struct (defaults and range checks) and one arm in
//! each of the `match`es on the key below.
//!
//! A stored value is sparse: it holds only the fields an admin set, and a
//! field that is absent uses the code default. Each instance keeps a typed
//! snapshot of the effective values, reloaded on a timer and refreshed
//! immediately after an update handled by this instance. Consumers read the
//! typed snapshot, never the JSON.
//!
//! The table is stored in plaintext (approved in `database_encryption.rs`):
//! settings here must be operational tuning values only. Never add a setting
//! that holds secrets, credentials, or customer data.

pub mod ports;

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use inference_providers::{DisabledSources, ProviderSource};
use placement::Tuning;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

pub use ports::{AdminSettingsRepository, StoredSetting};

/// How often each instance re-reads the stored settings.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Every key a setting can be stored under.
pub const KNOWN_KEYS: &[&str] = &[KEY_PLACEMENT, KEY_ATTESTED_3P];

pub const KEY_PLACEMENT: &str = "placement";
pub const KEY_ATTESTED_3P: &str = "attested_3p";

#[derive(Debug, thiserror::Error)]
pub enum AdminSettingsError {
    #[error("unknown setting")]
    UnknownKey,
    #[error("{0}")]
    Invalid(String),
    #[error("settings storage failed")]
    Storage(#[source] anyhow::Error),
}

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

/// The typed snapshot of every setting's effective value.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AdminSettings {
    pub placement: PlacementTuning,
    pub attested_3p: Attested3pSettings,
}

/// One setting as an admin sees it: the effective value, and when and by
/// whom it was last changed (absent until first changed).
#[derive(Debug, Clone, PartialEq)]
pub struct SettingView {
    pub key: &'static str,
    pub value: Value,
    pub updated_at: Option<DateTime<Utc>>,
    pub updated_by_user_id: Option<Uuid>,
}

/// Deserializes and validates a stored (sparse) value: stored fields over
/// the defaults. The one place that knows each key's type.
fn parse(key: &str, stored: Option<&Value>) -> Result<ParsedSetting, AdminSettingsError> {
    match key {
        KEY_PLACEMENT => {
            let typed: PlacementTuning = match stored {
                Some(v) => serde_json::from_value(v.clone())
                    .map_err(|e| AdminSettingsError::Invalid(e.to_string()))?,
                None => PlacementTuning::default(),
            };
            typed.validate()?;
            Ok(ParsedSetting::Placement(typed))
        }
        KEY_ATTESTED_3P => {
            let typed: Attested3pSettings = match stored {
                Some(v) => serde_json::from_value(v.clone())
                    .map_err(|e| AdminSettingsError::Invalid(e.to_string()))?,
                None => Attested3pSettings::default(),
            };
            Ok(ParsedSetting::Attested3p(typed))
        }
        _ => Err(AdminSettingsError::UnknownKey),
    }
}

enum ParsedSetting {
    Placement(PlacementTuning),
    Attested3p(Attested3pSettings),
}

impl ParsedSetting {
    fn effective_json(&self) -> Value {
        match self {
            ParsedSetting::Placement(p) => serde_json::to_value(p).unwrap_or(Value::Null),
            ParsedSetting::Attested3p(a) => serde_json::to_value(a).unwrap_or(Value::Null),
        }
    }
}

/// The field names a stored value can hold under `key`.
fn known_fields(key: &str) -> &'static [&'static str] {
    match key {
        KEY_PLACEMENT => PLACEMENT_FIELDS,
        KEY_ATTESTED_3P => ATTESTED_3P_FIELDS,
        _ => &[],
    }
}

fn known_key(key: &str) -> Result<&'static str, AdminSettingsError> {
    KNOWN_KEYS
        .iter()
        .find(|k| **k == key)
        .copied()
        .ok_or(AdminSettingsError::UnknownKey)
}

/// The new sparse stored value: `stored` with `patch` applied field by
/// field (`null` removes a field, so it falls back to its default), checked
/// as a whole. Nothing is stored when it fails.
fn merge(key: &str, stored: Option<&Value>, patch: &Value) -> Result<Value, AdminSettingsError> {
    let Value::Object(patch) = patch else {
        return Err(AdminSettingsError::Invalid(
            "the body must be a JSON object".to_string(),
        ));
    };
    // Checked before `null` resets are applied, so a field name outside the
    // schema never gets past here, and never reaches a log line.
    let known = known_fields(key);
    if patch.keys().any(|f| !known.contains(&f.as_str())) {
        return Err(AdminSettingsError::Invalid(
            "the body has a field that is not a knob of this setting".to_string(),
        ));
    }
    let mut merged: Map<String, Value> = match stored {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    for (field, value) in patch {
        if value.is_null() {
            merged.remove(field);
        } else {
            merged.insert(field.clone(), value.clone());
        }
    }
    let merged = Value::Object(merged);
    parse(key, Some(&merged))?;
    Ok(merged)
}

fn view(
    key: &'static str,
    stored: Option<&StoredSetting>,
) -> Result<SettingView, AdminSettingsError> {
    Ok(SettingView {
        key,
        value: parse(key, stored.map(|s| &s.value))?.effective_json(),
        updated_at: stored.map(|s| s.updated_at),
        updated_by_user_id: stored.and_then(|s| s.updated_by_user_id),
    })
}

/// The typed snapshot from stored rows. A key with no row, or whose stored
/// value no longer passes its checks, uses its defaults.
fn snapshot(rows: &[StoredSetting]) -> AdminSettings {
    let mut s = AdminSettings::default();
    for row in rows {
        match parse(&row.key, Some(&row.value)) {
            Ok(ParsedSetting::Placement(p)) => s.placement = p,
            Ok(ParsedSetting::Attested3p(a)) => s.attested_3p = a,
            Err(AdminSettingsError::UnknownKey) => {}
            Err(_) => tracing::warn!(
                setting = %row.key,
                "Stored admin setting is invalid; using its defaults"
            ),
        }
    }
    s
}

pub struct AdminSettingsService {
    repository: Arc<dyn AdminSettingsRepository>,
    current: ArcSwap<AdminSettings>,
    /// The handle the placers read (`InferenceProviderPool::placement_tuning`).
    placement: Arc<ArcSwap<Tuning>>,
    /// The handle the pool reads (`InferenceProviderPool::attested_3p_disabled`).
    attested_3p: Arc<ArcSwap<DisabledSources>>,
    /// Held across every read-then-apply of the snapshot (`update` and
    /// `reload`), so an older read can never be applied after a newer one.
    refresh: tokio::sync::Mutex<()>,
}

impl AdminSettingsService {
    pub fn new(
        repository: Arc<dyn AdminSettingsRepository>,
        placement: Arc<ArcSwap<Tuning>>,
        attested_3p: Arc<ArcSwap<DisabledSources>>,
    ) -> Self {
        Self {
            repository,
            current: ArcSwap::from_pointee(AdminSettings::default()),
            placement,
            attested_3p,
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    /// The effective placement tuning on this instance.
    pub fn placement(&self) -> PlacementTuning {
        self.current.load().placement
    }

    fn apply(&self, s: AdminSettings) {
        self.placement.store(Arc::new(s.placement.to_tuning()));
        self.attested_3p
            .store(Arc::new(s.attested_3p.disabled_sources));
        self.current.store(Arc::new(s));
    }

    /// Every known setting with its effective value, in key order.
    pub async fn get_all(&self) -> Result<Vec<SettingView>, AdminSettingsError> {
        let rows = self
            .repository
            .get_all()
            .await
            .map_err(AdminSettingsError::Storage)?;
        KNOWN_KEYS
            .iter()
            .map(|key| view(key, rows.iter().find(|r| r.key == *key)))
            .collect()
    }

    pub async fn get(&self, key: &str) -> Result<SettingView, AdminSettingsError> {
        let key = known_key(key)?;
        let stored = self
            .repository
            .get(key)
            .await
            .map_err(AdminSettingsError::Storage)?;
        view(key, stored.as_ref())
    }

    /// Applies `patch` to the setting under `key`, stores the result, and
    /// makes it live on this instance. Nothing is stored when the result
    /// fails its checks.
    pub async fn update(
        &self,
        key: &str,
        patch: Value,
        by_user: Uuid,
    ) -> Result<SettingView, AdminSettingsError> {
        let key = known_key(key)?;
        let _refresh = self.refresh.lock().await;
        let stored = self
            .repository
            .get(key)
            .await
            .map_err(AdminSettingsError::Storage)?;
        let merged = merge(key, stored.as_ref().map(|s| &s.value), &patch)?;
        let saved = self
            .repository
            .upsert(key, merged, by_user)
            .await
            .map_err(AdminSettingsError::Storage)?;
        // Applied from the value just saved, not a re-read: the write has
        // committed, so nothing after it may fail the request.
        match parse(key, Some(&saved.value)) {
            Ok(ParsedSetting::Placement(p)) => {
                let mut s = **self.current.load();
                s.placement = p;
                self.apply(s);
            }
            Ok(ParsedSetting::Attested3p(a)) => {
                let mut s = **self.current.load();
                s.attested_3p = a;
                self.apply(s);
            }
            Err(_) => {}
        }
        let names: Vec<&String> = patch
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        tracing::info!(
            admin_user_id = %by_user,
            setting = key,
            fields = ?names,
            "Admin setting updated"
        );
        view(key, Some(&saved))
    }

    /// Reads every stored setting into the live snapshot. A read failure
    /// keeps the last good values.
    pub async fn reload(&self) {
        let _refresh = self.refresh.lock().await;
        match self.repository.get_all().await {
            Ok(rows) => self.apply(snapshot(&rows)),
            Err(e) => tracing::warn!(
                error = %e,
                "Failed to read admin settings; keeping the current values"
            ),
        }
    }

    /// Loads once, then reloads every [`RELOAD_INTERVAL`] on a background
    /// task. Until the first successful read the values are the defaults.
    pub async fn start(self: Arc<Self>) {
        self.reload().await;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(RELOAD_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                self.reload().await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRepo {
        rows: Mutex<BTreeMap<String, StoredSetting>>,
        fail_reads: Mutex<bool>,
        fail_get_all: Mutex<bool>,
        upserts: Mutex<u32>,
    }

    impl FakeRepo {
        fn put(&self, key: &str, value: Value) {
            self.rows.lock().unwrap().insert(
                key.to_string(),
                StoredSetting {
                    key: key.to_string(),
                    value,
                    updated_by_user_id: None,
                    updated_at: Utc::now(),
                },
            );
        }
    }

    #[async_trait]
    impl AdminSettingsRepository for FakeRepo {
        async fn get_all(&self) -> anyhow::Result<Vec<StoredSetting>> {
            if *self.fail_reads.lock().unwrap() || *self.fail_get_all.lock().unwrap() {
                return Err(anyhow!("db down"));
            }
            Ok(self.rows.lock().unwrap().values().cloned().collect())
        }

        async fn get(&self, key: &str) -> anyhow::Result<Option<StoredSetting>> {
            if *self.fail_reads.lock().unwrap() {
                return Err(anyhow!("db down"));
            }
            Ok(self.rows.lock().unwrap().get(key).cloned())
        }

        async fn upsert(
            &self,
            key: &str,
            value: Value,
            by_user: Uuid,
        ) -> anyhow::Result<StoredSetting> {
            *self.upserts.lock().unwrap() += 1;
            let row = StoredSetting {
                key: key.to_string(),
                value,
                updated_by_user_id: Some(by_user),
                updated_at: Utc::now(),
            };
            self.rows
                .lock()
                .unwrap()
                .insert(key.to_string(), row.clone());
            Ok(row)
        }
    }

    fn service() -> (AdminSettingsService, Arc<FakeRepo>, Arc<ArcSwap<Tuning>>) {
        let repo = Arc::new(FakeRepo::default());
        let handle = Arc::new(ArcSwap::from_pointee(Tuning::default()));
        let svc = AdminSettingsService::new(
            repo.clone(),
            handle.clone(),
            Arc::new(ArcSwap::from_pointee(DisabledSources::default())),
        );
        (svc, repo, handle)
    }

    #[test]
    fn attested_3p_parses_and_rejects_unknown_sources() {
        let ok = parse(
            KEY_ATTESTED_3P,
            Some(&json!({"disabled_sources": ["tinfoil"]})),
        )
        .unwrap();
        assert!(
            matches!(ok, ParsedSetting::Attested3p(s) if s.disabled_sources.contains(ProviderSource::Tinfoil))
        );
        assert!(parse(
            KEY_ATTESTED_3P,
            Some(&json!({"disabled_sources": ["vllm"]}))
        )
        .is_err());
        assert!(parse(KEY_ATTESTED_3P, Some(&json!({"other": 1}))).is_err());
        assert_eq!(
            parse(KEY_ATTESTED_3P, None).unwrap().effective_json(),
            json!({"disabled_sources": []})
        );
    }

    #[tokio::test]
    async fn attested_3p_update_and_reload_push_to_the_handle() {
        let repo = Arc::new(FakeRepo::default());
        let handle = Arc::new(ArcSwap::from_pointee(DisabledSources::default()));
        let svc = AdminSettingsService::new(
            repo.clone(),
            Arc::new(ArcSwap::from_pointee(Tuning::default())),
            handle.clone(),
        );
        let v = svc
            .update(
                KEY_ATTESTED_3P,
                json!({"disabled_sources": ["chutes"]}),
                Uuid::new_v4(),
            )
            .await
            .unwrap();
        assert_eq!(v.value, json!({"disabled_sources": ["chutes"]}));
        assert!(handle.load().contains(ProviderSource::Chutes));
        assert!(!handle.load().contains(ProviderSource::Tinfoil));

        repo.put(KEY_ATTESTED_3P, json!({"disabled_sources": ["tinfoil"]}));
        svc.reload().await;
        assert!(handle.load().contains(ProviderSource::Tinfoil));
        assert!(!handle.load().contains(ProviderSource::Chutes));

        let err = svc
            .update(
                KEY_ATTESTED_3P,
                json!({"disabled_sources": ["external"]}),
                Uuid::new_v4(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, AdminSettingsError::Invalid(m) if m == "disabled_sources accepts: chutes, tinfoil")
        );
    }

    async fn invalid(svc: &AdminSettingsService, patch: Value) {
        let r = svc
            .update(KEY_PLACEMENT, patch.clone(), Uuid::new_v4())
            .await;
        assert!(
            matches!(r, Err(AdminSettingsError::Invalid(_))),
            "{patch} should be rejected, got {r:?}"
        );
    }

    #[tokio::test]
    async fn unknown_key_is_rejected() {
        let (svc, repo, _) = service();
        assert!(matches!(
            svc.get("nope").await,
            Err(AdminSettingsError::UnknownKey)
        ));
        assert!(matches!(
            svc.update("nope", json!({"a": 1}), Uuid::new_v4()).await,
            Err(AdminSettingsError::UnknownKey)
        ));
        assert_eq!(*repo.upserts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn every_knob_rejects_out_of_range_values() {
        let (svc, repo, handle) = service();
        for (knob, bad) in [
            (
                "affinity_abs_slack",
                vec![json!(4.01), json!(-0.01), json!(-1)],
            ),
            ("affinity_eps", vec![json!(2.01), json!(-0.01), json!(-1)]),
            (
                "kv_max",
                vec![json!(0.49), json!(1.01), json!(-1), json!(0)],
            ),
            (
                "lane_load_tokens",
                vec![json!(3_999), json!(1_000_001), json!(0), json!(-1)],
            ),
            (
                "pin_hold_factor",
                vec![json!(0.49), json!(2.01), json!(0), json!(-1)],
            ),
            (
                "pin_ttl_ms",
                vec![json!(59_999), json!(3_600_001), json!(0), json!(-1)],
            ),
        ] {
            for v in bad {
                invalid(&svc, json!({ knob: v })).await;
            }
        }
        // Wrong types and unknown fields are rejected too.
        invalid(&svc, json!({"kv_max": "high"})).await;
        invalid(&svc, json!({"lane_load_tokens": 5000.5})).await;
        invalid(&svc, json!({"enabled": "no"})).await;
        invalid(&svc, json!({"enabled": 0})).await;
        invalid(&svc, json!({"not_a_knob": 1})).await;
        // A null-valued unknown field is rejected too (not silently dropped).
        invalid(&svc, json!({"not_a_knob": null})).await;
        invalid(&svc, json!([1])).await;
        assert_eq!(*repo.upserts.lock().unwrap(), 0, "nothing stored");
        assert_eq!(**handle.load(), Tuning::default());
    }

    #[test]
    fn placement_fields_match_the_struct() {
        let v = serde_json::to_value(PlacementTuning::default()).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort();
        let mut want = PLACEMENT_FIELDS.to_vec();
        want.sort();
        assert_eq!(keys, want);
    }

    #[tokio::test]
    async fn update_succeeds_and_applies_when_reads_after_the_write_fail() {
        let (svc, repo, handle) = service();
        // Reads fail after the first (the pre-write get): emulate by failing
        // get_all only; update must not depend on it.
        *repo.fail_get_all.lock().unwrap() = true;
        let v = svc
            .update(KEY_PLACEMENT, json!({"kv_max": 0.9}), Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(v.value["kv_max"], json!(0.9));
        assert_eq!(handle.load().kv_max, 0.9);
    }

    #[test]
    fn nan_and_infinity_fail_validation() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for tuning in [
                PlacementTuning {
                    affinity_abs_slack: bad,
                    ..Default::default()
                },
                PlacementTuning {
                    affinity_eps: bad,
                    ..Default::default()
                },
                PlacementTuning {
                    kv_max: bad,
                    ..Default::default()
                },
                PlacementTuning {
                    pin_hold_factor: bad,
                    ..Default::default()
                },
            ] {
                assert!(tuning.validate().is_err());
            }
        }
    }

    #[test]
    fn range_bounds_are_inclusive() {
        let lo = PlacementTuning {
            affinity_abs_slack: 0.0,
            affinity_eps: 0.0,
            kv_max: 0.5,
            lane_load_tokens: 4_000,
            pin_hold_factor: 0.5,
            pin_ttl_ms: 60_000,
            enabled: false,
        };
        let hi = PlacementTuning {
            affinity_abs_slack: 4.0,
            affinity_eps: 2.0,
            kv_max: 1.0,
            lane_load_tokens: 1_000_000,
            pin_hold_factor: 2.0,
            pin_ttl_ms: 3_600_000,
            enabled: true,
        };
        assert!(lo.validate().is_ok());
        assert!(hi.validate().is_ok());
        assert!(PlacementTuning::default().validate().is_ok());
        assert_eq!(PlacementTuning::default().to_tuning(), Tuning::default());
    }

    #[tokio::test]
    async fn defaults_when_nothing_is_stored() {
        let (svc, _, _) = service();
        let v = svc.get(KEY_PLACEMENT).await.unwrap();
        assert_eq!(
            v.value,
            json!({
                "affinity_abs_slack": 0.25, "affinity_eps": 0.25, "kv_max": 0.95,
                "lane_load_tokens": 64_000, "pin_hold_factor": 1.0,
                "pin_ttl_ms": 600_000, "enabled": true
            })
        );
        assert_eq!(v.updated_at, None);
        assert_eq!(v.updated_by_user_id, None);
    }

    #[tokio::test]
    async fn partial_patches_merge_and_null_resets() {
        let (svc, repo, handle) = service();
        let admin = Uuid::new_v4();
        let d = Tuning::default();

        let v = svc
            .update(KEY_PLACEMENT, json!({"kv_max": 0.9}), admin)
            .await
            .unwrap();
        assert_eq!(v.value["kv_max"], json!(0.9));
        assert_eq!(v.updated_by_user_id, Some(admin));
        // Only the field that was set is stored.
        assert_eq!(
            repo.rows.lock().unwrap()[KEY_PLACEMENT].value,
            json!({"kv_max": 0.9})
        );

        // A second patch to another field keeps the first.
        let v = svc
            .update(
                KEY_PLACEMENT,
                json!({"pin_ttl_ms": 120_000, "pin_hold_factor": 1.25}),
                admin,
            )
            .await
            .unwrap();
        assert_eq!(v.value["pin_hold_factor"], json!(1.25));
        assert_eq!(v.value["kv_max"], json!(0.9));
        assert_eq!(v.value["pin_ttl_ms"], json!(120_000));
        assert_eq!(v.value["affinity_eps"], json!(0.25));
        assert_eq!(
            **handle.load(),
            Tuning {
                kv_max: 0.9,
                pin_hold_factor: 1.25,
                pin_ttl_ms: 120_000,
                ..d
            },
            "applied to the live handle"
        );
        assert_eq!(svc.placement().kv_max, 0.9);

        // null resets one field; the other keeps its value.
        let v = svc
            .update(KEY_PLACEMENT, json!({"kv_max": null}), admin)
            .await
            .unwrap();
        assert_eq!(v.value["kv_max"], json!(0.95));
        assert_eq!(v.value["pin_ttl_ms"], json!(120_000));
        assert_eq!(
            repo.rows.lock().unwrap()[KEY_PLACEMENT].value,
            json!({"pin_ttl_ms": 120_000, "pin_hold_factor": 1.25})
        );
        assert_eq!(
            **handle.load(),
            Tuning {
                pin_hold_factor: 1.25,
                pin_ttl_ms: 120_000,
                ..d
            }
        );
    }

    #[tokio::test]
    async fn enabled_round_trips_and_null_resets() {
        let (svc, _, handle) = service();
        let admin = Uuid::new_v4();
        let v = svc
            .update(KEY_PLACEMENT, json!({"enabled": false}), admin)
            .await
            .unwrap();
        assert_eq!(v.value["enabled"], json!(false));
        assert!(!handle.load().enabled);
        assert!(!svc.placement().enabled);

        let v = svc
            .update(KEY_PLACEMENT, json!({"enabled": null}), admin)
            .await
            .unwrap();
        assert_eq!(v.value["enabled"], json!(true));
        assert_eq!(**handle.load(), Tuning::default());
    }

    #[tokio::test]
    async fn get_all_lists_every_known_key_with_effective_values() {
        let (svc, _, _) = service();
        svc.update(KEY_PLACEMENT, json!({"affinity_eps": 0.5}), Uuid::new_v4())
            .await
            .unwrap();
        let all = svc.get_all().await.unwrap();
        assert_eq!(all.len(), KNOWN_KEYS.len());
        assert!(all.iter().any(|v| v.key == KEY_ATTESTED_3P));
        assert_eq!(all[0].key, KEY_PLACEMENT);
        assert_eq!(all[0].value["affinity_eps"], json!(0.5));
        assert_eq!(all[0].value["kv_max"], json!(0.95));
        assert!(all[0].updated_at.is_some());
    }

    #[tokio::test]
    async fn reload_applies_rows_keeps_last_good_and_survives_bad_rows() {
        let (svc, repo, handle) = service();
        // Another instance wrote the row.
        repo.put(KEY_PLACEMENT, json!({"lane_load_tokens": 10_000}));
        svc.reload().await;
        let applied = Tuning {
            lane_load_tokens: 10_000,
            ..Tuning::default()
        };
        assert_eq!(**handle.load(), applied);

        *repo.fail_reads.lock().unwrap() = true;
        svc.reload().await;
        assert_eq!(
            **handle.load(),
            applied,
            "a failed read keeps the last good values"
        );

        // A stored value that no longer passes its checks uses the defaults,
        // as does a deleted row; a row for an unknown key is ignored.
        *repo.fail_reads.lock().unwrap() = false;
        repo.put(KEY_PLACEMENT, json!({"kv_max": 7}));
        repo.put("removed_setting", json!({"x": 1}));
        svc.reload().await;
        assert_eq!(**handle.load(), Tuning::default());
        repo.rows.lock().unwrap().remove(KEY_PLACEMENT);
        svc.reload().await;
        assert_eq!(**handle.load(), Tuning::default());
    }
}
