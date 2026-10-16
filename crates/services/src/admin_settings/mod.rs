//! Admin-changeable runtime settings. Each setting is a JSON value under a
//! key in one table, so adding a setting needs neither a migration nor an
//! endpoint: its owner implements [`SettingKey`] (typed value, defaults,
//! range checks, the handle it publishes to) and registers it on
//! [`AdminSettingsService::builder`]. This module knows no key's type.
//!
//! A stored value is sparse: it holds only the fields an admin set, and a
//! field that is absent uses the code default. Each instance keeps the typed
//! effective value of every key, reloaded on a timer and refreshed
//! immediately after an update handled by this instance. Consumers read the
//! typed value, never the JSON.
//!
//! This is the runtime KV store for operator-set configuration: tuning knobs,
//! kill switches and public signed trust inputs (which their owner verifies
//! before use). Runtime-changeable config goes here as a registered key; never
//! add a table, migration or admin API for it. Consumers that need a setting
//! at startup register after this service has loaded.
//!
//! The table is stored in plaintext (approved in `database_encryption.rs`):
//! never add a setting that holds secrets, credentials, or customer data.

mod keys;
pub mod ports;
pub mod registry;

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use inference_providers::DisabledSources;
use placement::Tuning;
use serde_json::{Map, Value};
use uuid::Uuid;

pub use keys::{Attested3pKey, Attested3pSettings, PlacementKey, PlacementTuning};
pub use ports::{AdminSettingsRepository, StoredSetting};
pub use registry::{AdminSettingsBuilder, SettingKey, Versioned};

use registry::ErasedKey;

/// How often each instance re-reads the stored settings.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub const KEY_PLACEMENT: &str = PlacementKey::NAME;
pub const KEY_ATTESTED_3P: &str = Attested3pKey::NAME;

#[derive(Debug, thiserror::Error)]
pub enum AdminSettingsError {
    #[error("unknown setting")]
    UnknownKey,
    #[error("{0}")]
    Invalid(String),
    #[error("settings storage failed")]
    Storage(#[source] anyhow::Error),
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

/// The new sparse stored value: `stored` with `patch` applied field by
/// field (`null` removes a field, so it falls back to its default), checked
/// as a whole. Nothing is stored when it fails.
fn merge(
    key: &dyn ErasedKey,
    stored: Option<&Value>,
    patch: &Value,
) -> Result<Value, AdminSettingsError> {
    let Value::Object(patch) = patch else {
        return Err(AdminSettingsError::Invalid(
            "the body must be a JSON object".to_string(),
        ));
    };
    // Checked before `null` resets are applied, so a field name outside the
    // schema never gets past here, and never reaches a log line.
    let known = key.fields();
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
    let patch = Value::Object(patch.clone());
    if serde_json::to_vec(&merged).map_or(true, |b| b.len() > key.max_bytes()) {
        return Err(AdminSettingsError::Invalid(
            "the setting value is too large".to_string(),
        ));
    }
    key.check_patch(&patch, &merged)
        .map_err(AdminSettingsError::Invalid)?;
    Ok(merged)
}

fn view(
    key: &dyn ErasedKey,
    stored: Option<&StoredSetting>,
) -> Result<SettingView, AdminSettingsError> {
    Ok(SettingView {
        key: key.name(),
        value: key
            .effective_json(stored.map(|s| &s.value))
            .map_err(AdminSettingsError::Invalid)?,
        updated_at: stored.map(|s| s.updated_at),
        updated_by_user_id: stored.and_then(|s| s.updated_by_user_id),
    })
}

pub struct AdminSettingsService {
    repository: Arc<dyn AdminSettingsRepository>,
    /// Registered keys, in registration order.
    keys: Vec<Box<dyn ErasedKey>>,
    /// Held across every read-then-apply of the typed values (`update` and
    /// `reload`), so an older read can never be applied after a newer one.
    refresh: tokio::sync::Mutex<()>,
}

impl AdminSettingsService {
    pub fn builder(repository: Arc<dyn AdminSettingsRepository>) -> AdminSettingsBuilder {
        AdminSettingsBuilder::new(repository)
    }

    /// The two built-in keys: `placement` (the handle the placers read,
    /// `InferenceProviderPool::placement_tuning`) and `attested_3p` (the
    /// handle the pool reads, `InferenceProviderPool::attested_3p_disabled`).
    pub fn new(
        repository: Arc<dyn AdminSettingsRepository>,
        placement: Arc<ArcSwap<Tuning>>,
        attested_3p: Arc<ArcSwap<DisabledSources>>,
    ) -> Self {
        Self::builder(repository)
            .register::<PlacementKey>(placement)
            .register::<Attested3pKey>(attested_3p)
            .build()
    }

    fn from_parts(
        repository: Arc<dyn AdminSettingsRepository>,
        keys: Vec<Box<dyn ErasedKey>>,
    ) -> Self {
        Self {
            repository,
            keys,
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    fn key(&self, name: &str) -> Result<&dyn ErasedKey, AdminSettingsError> {
        self.keys
            .iter()
            .find(|k| k.name() == name)
            .map(|k| k.as_ref())
            .ok_or(AdminSettingsError::UnknownKey)
    }

    /// The effective value of `K` on this instance (its default when `K` is
    /// not registered).
    pub fn current<K: SettingKey>(&self) -> K::Value {
        self.key(K::NAME)
            .ok()
            .and_then(|k| k.current_any().downcast::<K::Value>().ok())
            .map(|v| *v)
            .unwrap_or_else(K::default_value)
    }

    /// The version of `K`'s effective value on this instance: 0 until a
    /// change makes it differ, then incremented per change (0 when `K` is
    /// not registered).
    pub fn version<K: SettingKey>(&self) -> u64 {
        self.key(K::NAME).map_or(0, |k| k.version())
    }

    /// The effective placement tuning on this instance.
    pub fn placement(&self) -> PlacementTuning {
        self.current::<PlacementKey>()
    }

    /// Every known setting with its effective value, in registration order.
    pub async fn get_all(&self) -> Result<Vec<SettingView>, AdminSettingsError> {
        let rows = self
            .repository
            .get_all()
            .await
            .map_err(AdminSettingsError::Storage)?;
        self.keys
            .iter()
            .map(|key| view(key.as_ref(), rows.iter().find(|r| r.key == key.name())))
            .collect()
    }

    pub async fn get(&self, key: &str) -> Result<SettingView, AdminSettingsError> {
        let key = self.key(key)?;
        let stored = self
            .repository
            .get(key.name())
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
        let key = self.key(key)?;
        let _refresh = self.refresh.lock().await;
        let stored = self
            .repository
            .get(key.name())
            .await
            .map_err(AdminSettingsError::Storage)?;
        let merged = merge(key, stored.as_ref().map(|s| &s.value), &patch)?;
        let saved = self
            .repository
            .upsert(key.name(), merged, by_user)
            .await
            .map_err(AdminSettingsError::Storage)?;
        // Applied from the value just saved, not a re-read: the write has
        // committed, so nothing after it may fail the request.
        key.apply(Some(&saved), false);
        let names: Vec<&String> = patch
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        tracing::info!(
            admin_user_id = %by_user,
            setting = key.name(),
            fields = ?names,
            "Admin setting updated"
        );
        view(key, Some(&saved))
    }

    /// Reads every stored setting into the live values. A read failure
    /// keeps the last good values. A key with no row, or whose stored value
    /// no longer passes its checks, uses its defaults; a row for an
    /// unregistered key is ignored.
    pub async fn reload(&self) {
        let _refresh = self.refresh.lock().await;
        match self.repository.get_all().await {
            Ok(rows) => {
                for key in &self.keys {
                    key.apply(rows.iter().find(|r| r.key == key.name()), true);
                }
            }
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
    use inference_providers::ProviderSource;
    use serde::{Deserialize, Serialize};
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
        let ok = Attested3pKey::parse(&json!({"disabled_sources": ["tinfoil"]}), true).unwrap();
        assert!(ok.disabled_sources.contains(ProviderSource::Tinfoil));
        assert!(Attested3pKey::parse(&json!({"disabled_sources": ["vllm"]}), true).is_err());
        assert!(Attested3pKey::parse(&json!({"other": 1}), true).is_err());
        assert_eq!(
            serde_json::to_value(Attested3pKey::default_value()).unwrap(),
            json!({"disabled_sources": []})
        );
    }

    #[tokio::test]
    async fn stored_row_with_unknown_source_keeps_known_ones_disabled() {
        let repo = Arc::new(FakeRepo::default());
        let handle = Arc::new(ArcSwap::from_pointee(DisabledSources::default()));
        let svc = AdminSettingsService::new(
            repo.clone(),
            Arc::new(ArcSwap::from_pointee(Tuning::default())),
            handle.clone(),
        );
        repo.put(
            KEY_ATTESTED_3P,
            json!({"disabled_sources": ["tinfoil", "future_x"], "future_field": 1}),
        );
        svc.reload().await;
        assert!(handle.load().contains(ProviderSource::Tinfoil));
        assert!(!handle.load().contains(ProviderSource::Chutes));
        // PATCH input stays strict.
        let err = svc
            .update(
                KEY_ATTESTED_3P,
                json!({"disabled_sources": ["future_x"]}),
                Uuid::new_v4(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdminSettingsError::Invalid(_)));
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
        let mut want = PlacementKey::FIELDS.to_vec();
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
        assert_eq!(all.len(), 2);
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

    // ---- registry behaviour: versions, size cap, cheap reload ----

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    struct TestVal {
        n: i64,
        s: String,
    }

    macro_rules! test_key {
        ($name:ident, $key:literal, $max:expr, $parses:ident) => {
            static $parses: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            struct $name;
            impl SettingKey for $name {
                const NAME: &'static str = $key;
                const FIELDS: &'static [&'static str] = &["n", "s"];
                const MAX_BYTES: usize = $max;
                type Value = TestVal;
                type Published = Versioned<TestVal>;
                fn default_value() -> TestVal {
                    TestVal::default()
                }
                fn parse(value: &Value, _strict: bool) -> Result<TestVal, String> {
                    $parses.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    serde_json::from_value(value.clone()).map_err(|e| e.to_string())
                }
                fn publish(value: &TestVal, version: u64) -> Versioned<TestVal> {
                    Versioned {
                        version,
                        value: value.clone(),
                    }
                }
            }
        };
    }
    test_key!(VKey, "vkey", 64 * 1024, VKEY_PARSES);
    test_key!(CountKey, "count_key", 64 * 1024, COUNT_PARSES);
    test_key!(TinyKey, "tiny_key", 16, TINY_PARSES);

    static VALIDATE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct ValidatedKey;
    impl SettingKey for ValidatedKey {
        const NAME: &'static str = "validated_key";
        const FIELDS: &'static [&'static str] = &["n", "s"];
        type Value = TestVal;
        type Published = Versioned<TestVal>;
        fn default_value() -> TestVal {
            TestVal::default()
        }
        fn parse(value: &Value, _strict: bool) -> Result<TestVal, String> {
            serde_json::from_value(value.clone()).map_err(|e| e.to_string())
        }
        fn validate_patch(patch: &Value, merged: &TestVal) -> Result<(), String> {
            VALIDATE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if patch.get("s").is_some() && merged.s == "bad" {
                return Err("s must not be bad".to_string());
            }
            Ok(())
        }
        fn publish(value: &TestVal, version: u64) -> Versioned<TestVal> {
            Versioned {
                version,
                value: value.clone(),
            }
        }
    }

    fn versioned_service<K: SettingKey<Published = Versioned<TestVal>>>() -> (
        Arc<AdminSettingsService>,
        Arc<FakeRepo>,
        Arc<ArcSwap<Versioned<TestVal>>>,
    ) {
        let repo = Arc::new(FakeRepo::default());
        let handle = Arc::new(ArcSwap::from_pointee(Versioned {
            version: 0,
            value: TestVal::default(),
        }));
        let svc = AdminSettingsService::builder(repo.clone())
            .register::<K>(handle.clone())
            .build();
        (Arc::new(svc), repo, handle)
    }

    #[tokio::test]
    async fn noop_patch_does_not_bump_version() {
        let (svc, _, handle) = versioned_service::<VKey>();
        let admin = Uuid::new_v4();
        svc.update("vkey", json!({"n": 0}), admin).await.unwrap();
        assert_eq!(handle.load().version, 0, "a field set to its default");
        svc.update("vkey", json!({"n": 0}), admin).await.unwrap();
        svc.update("vkey", json!({"s": null}), admin).await.unwrap();
        assert_eq!(handle.load().version, 0);
    }

    #[tokio::test]
    async fn effective_change_bumps_version_once() {
        let (svc, _, handle) = versioned_service::<VKey>();
        let admin = Uuid::new_v4();
        svc.update("vkey", json!({"n": 5}), admin).await.unwrap();
        assert_eq!(handle.load().version, 1);
        assert_eq!(handle.load().value.n, 5);
        // Same value again: no bump.
        svc.update("vkey", json!({"n": 5}), admin).await.unwrap();
        assert_eq!(handle.load().version, 1);
        svc.update("vkey", json!({"n": null}), admin).await.unwrap();
        assert_eq!(handle.load().version, 2);
        assert_eq!(svc.current::<VKey>(), TestVal::default());
    }

    #[tokio::test]
    async fn reload_skips_unchanged_rows() {
        let (svc, repo, handle) = versioned_service::<CountKey>();
        repo.put("count_key", json!({"n": 3}));
        let base = COUNT_PARSES.load(std::sync::atomic::Ordering::SeqCst);
        svc.reload().await;
        svc.reload().await;
        assert_eq!(
            COUNT_PARSES.load(std::sync::atomic::Ordering::SeqCst) - base,
            1,
            "an unchanged updated_at is parsed once"
        );
        assert_eq!(handle.load().value.n, 3);
        assert_eq!(handle.load().version, 1);
        repo.put("count_key", json!({"n": 4}));
        svc.reload().await;
        assert_eq!(
            COUNT_PARSES.load(std::sync::atomic::Ordering::SeqCst) - base,
            2
        );
        assert_eq!(handle.load().value.n, 4);
        // A deleted row is back to the default.
        repo.rows.lock().unwrap().remove("count_key");
        svc.reload().await;
        assert_eq!(handle.load().value, TestVal::default());
        assert_eq!(handle.load().version, 3);
    }

    #[tokio::test]
    async fn patch_over_max_bytes_is_rejected() {
        let (svc, repo, handle) = versioned_service::<TinyKey>();
        svc.update("tiny_key", json!({"s": "abc"}), Uuid::new_v4())
            .await
            .unwrap();
        let err = svc
            .update(
                "tiny_key",
                json!({"s": "a string well over sixteen bytes"}),
                Uuid::new_v4(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, AdminSettingsError::Invalid(m) if m == "the setting value is too large")
        );
        assert_eq!(*repo.upserts.lock().unwrap(), 1, "nothing more stored");
        assert_eq!(handle.load().value.s, "abc");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn version_monotonic_under_concurrent_patch_and_reload() {
        let (svc, repo, handle) = versioned_service::<VKey>();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = {
            let handle = handle.clone();
            let done = done.clone();
            tokio::spawn(async move {
                let mut last = 0;
                while !done.load(std::sync::atomic::Ordering::SeqCst) {
                    let v = handle.load().version;
                    assert!(v >= last, "version went backwards");
                    last = v;
                    tokio::task::yield_now().await;
                }
            })
        };
        let mut tasks = Vec::new();
        for i in 0..50i64 {
            let svc = svc.clone();
            tasks.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    svc.update("vkey", json!({"n": i + 1}), Uuid::new_v4())
                        .await
                        .unwrap();
                } else {
                    svc.reload().await;
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        watcher.await.unwrap();
        let stored = repo.rows.lock().unwrap()["vkey"].value.clone();
        assert_eq!(
            json!(handle.load().value.n),
            stored["n"],
            "the published value matches the latest committed row"
        );
    }

    #[tokio::test]
    async fn validate_patch_runs_after_parse_and_failure_stores_nothing() {
        use std::sync::atomic::Ordering;
        let (svc, repo, _) = versioned_service::<ValidatedKey>();
        let before = VALIDATE_CALLS.load(Ordering::SeqCst);
        let r = svc
            .update("validated_key", json!({"s": "bad"}), Uuid::new_v4())
            .await;
        assert!(matches!(&r, Err(AdminSettingsError::Invalid(m)) if m == "s must not be bad"));
        assert_eq!(VALIDATE_CALLS.load(Ordering::SeqCst), before + 1);
        assert!(repo.rows.lock().unwrap().is_empty(), "nothing stored");
        // A parse failure never reaches validate_patch.
        let r = svc
            .update("validated_key", json!({"n": "x"}), Uuid::new_v4())
            .await;
        assert!(matches!(r, Err(AdminSettingsError::Invalid(_))));
        assert_eq!(VALIDATE_CALLS.load(Ordering::SeqCst), before + 1);
        svc.update("validated_key", json!({"s": "ok"}), Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(repo.rows.lock().unwrap().len(), 1);
    }
}
