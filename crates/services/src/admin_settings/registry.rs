//! The setting-key registry. A [`SettingKey`] is a strategy owned by the
//! consumer of a setting: it names the key, lists its fields, parses its
//! value, and builds the handle value its owner reads. The admin settings
//! service holds registered keys type-erased and knows no key's type.

use std::any::Any;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{AdminSettingsRepository, AdminSettingsService, StoredSetting};

/// The conventional `Published` type for a new key: the effective value with
/// a per-key version. The version starts at 0 and increments when a change
/// makes the effective value differ from the current one, so a consumer can
/// tell by comparing versions whether to redo work.
#[derive(Debug, Clone, PartialEq)]
pub struct Versioned<T> {
    pub version: u64,
    pub value: T,
}

/// One admin-changeable setting, owned by its consumer.
pub trait SettingKey: Send + Sync + 'static {
    const NAME: &'static str;
    /// Sparse-merge allow-list (patch fields outside it are rejected before null resets).
    const FIELDS: &'static [&'static str];
    /// Serialized size cap of the merged value, checked at PATCH. Default 64 KiB.
    const MAX_BYTES: usize = 64 * 1024;
    /// The typed effective value (defaults filled in).
    type Value: Clone + PartialEq + serde::Serialize + Send + Sync + 'static;
    /// What the owner's handle holds (e.g. `Tuning`, `DisabledSources`).
    type Published: Send + Sync + 'static;
    fn default_value() -> Self::Value;
    /// `strict`: PATCH input, unknown values are errors. Not strict: a stored row,
    /// read leniently so a newer binary's row cannot reset known values.
    fn parse(value: &Value, strict: bool) -> Result<Self::Value, String>;
    /// Extra checks of a PATCH, run after the strict parse of the merged
    /// value succeeds and before anything is stored. `patch` is the request
    /// body, so a key can do costly checks only for the fields it sets. The
    /// default accepts.
    fn validate_patch(patch: &Value, merged: &Self::Value) -> Result<(), String> {
        let _ = (patch, merged);
        Ok(())
    }
    /// Build the published form. `version` increments on every effective change.
    fn publish(value: &Self::Value, version: u64) -> Self::Published;
}

/// A key with its type erased, so the service can hold keys of any type.
pub(super) trait ErasedKey: Send + Sync {
    fn name(&self) -> &'static str;
    fn fields(&self) -> &'static [&'static str];
    fn max_bytes(&self) -> usize;
    /// Strict parse of the merged PATCH value, then the key's
    /// [`SettingKey::validate_patch`].
    fn check_patch(&self, patch: &Value, merged: &Value) -> Result<(), String>;
    /// The per-key version `apply` last published.
    fn version(&self) -> u64;
    /// The effective value as JSON: the lenient parse of a stored value, or
    /// the default when there is none.
    fn effective_json(&self, stored: Option<&Value>) -> Result<Value, String>;
    /// Makes a stored row (or the default, when there is none) live. A
    /// stored value that no longer parses uses the default. With
    /// `skip_unchanged`, a row whose `updated_at` is the one last applied is
    /// not parsed again.
    fn apply(&self, row: Option<&StoredSetting>, skip_unchanged: bool);
    fn current_any(&self) -> Box<dyn Any>;
}

/// What `apply` last did, and the version it published.
struct State {
    /// `None`: nothing applied yet. `Some(None)`: applied with no row.
    /// `Some(Some(t))`: applied the row last updated at `t`.
    applied: Option<Option<DateTime<Utc>>>,
    version: u64,
}

struct Registered<K: SettingKey> {
    handle: Arc<ArcSwap<K::Published>>,
    current: ArcSwap<K::Value>,
    state: Mutex<State>,
}

impl<K: SettingKey> Registered<K> {
    fn new(handle: Arc<ArcSwap<K::Published>>) -> Self {
        Self {
            handle,
            current: ArcSwap::from_pointee(K::default_value()),
            state: Mutex::new(State {
                applied: None,
                version: 0,
            }),
        }
    }
}

impl<K: SettingKey> ErasedKey for Registered<K> {
    fn name(&self) -> &'static str {
        K::NAME
    }

    fn fields(&self) -> &'static [&'static str] {
        K::FIELDS
    }

    fn max_bytes(&self) -> usize {
        K::MAX_BYTES
    }

    fn check_patch(&self, patch: &Value, merged: &Value) -> Result<(), String> {
        let typed = K::parse(merged, true)?;
        K::validate_patch(patch, &typed)
    }

    fn version(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).version
    }

    fn effective_json(&self, stored: Option<&Value>) -> Result<Value, String> {
        let typed = match stored {
            Some(v) => K::parse(v, false)?,
            None => K::default_value(),
        };
        Ok(serde_json::to_value(&typed).unwrap_or(Value::Null))
    }

    fn apply(&self, row: Option<&StoredSetting>, skip_unchanged: bool) {
        let at = row.map(|r| r.updated_at);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if skip_unchanged && state.applied == Some(at) {
            return;
        }
        let typed = match row.map(|r| K::parse(&r.value, false)) {
            Some(Ok(typed)) => typed,
            Some(Err(_)) => {
                tracing::warn!(
                    setting = K::NAME,
                    "Stored admin setting is invalid; using its defaults"
                );
                K::default_value()
            }
            None => K::default_value(),
        };
        let first = state.applied.is_none();
        state.applied = Some(at);
        let changed = typed != **self.current.load();
        if !changed && !first {
            return;
        }
        if changed {
            state.version += 1;
        }
        self.handle
            .store(Arc::new(K::publish(&typed, state.version)));
        self.current.store(Arc::new(typed));
    }

    fn current_any(&self) -> Box<dyn Any> {
        Box::new(K::Value::clone(&self.current.load()))
    }
}

/// Registers keys, in order, and builds the service.
pub struct AdminSettingsBuilder {
    repository: Arc<dyn AdminSettingsRepository>,
    keys: Vec<Box<dyn ErasedKey>>,
}

impl AdminSettingsBuilder {
    pub(super) fn new(repository: Arc<dyn AdminSettingsRepository>) -> Self {
        Self {
            repository,
            keys: Vec::new(),
        }
    }

    /// Registers `K`, publishing into `handle`. Order is registration order
    /// (the order of `get_all`). Panics on a duplicate name: a programming
    /// error caught at startup.
    pub fn register<K: SettingKey>(mut self, handle: Arc<ArcSwap<K::Published>>) -> Self {
        assert!(
            !self.keys.iter().any(|k| k.name() == K::NAME),
            "admin setting key registered twice"
        );
        self.keys.push(Box::new(Registered::<K>::new(handle)));
        self
    }

    pub fn build(self) -> AdminSettingsService {
        AdminSettingsService::from_parts(self.repository, self.keys)
    }
}
