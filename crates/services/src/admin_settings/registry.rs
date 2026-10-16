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
/// a per-key version. Version 0 means "not loaded yet": the owner's initial
/// value, before any read of the store succeeded. The first successful apply
/// of the key always publishes version 1, even when the value equals the
/// initial one; after that the version increments when a change makes the
/// effective value differ from the current one. A consumer can tell by
/// comparing versions whether to redo work.
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
    /// Runs under the per-key state lock: cheap, non-blocking, and must not
    /// re-enter `AdminSettingsService`.
    fn parse(value: &Value, strict: bool) -> Result<Self::Value, String>;
    /// Build the published form. `version` increments on every effective change.
    ///
    /// `parse` and `publish` run while the key's per-key state lock is held.
    /// They must be cheap and non-blocking, and must not re-enter
    /// `AdminSettingsService`.
    fn publish(value: &Self::Value, version: u64) -> Self::Published;
}

/// A key with its type erased, so the service can hold keys of any type.
pub(super) trait ErasedKey: Send + Sync {
    fn name(&self) -> &'static str;
    fn fields(&self) -> &'static [&'static str];
    fn max_bytes(&self) -> usize;
    /// Strict parse of PATCH input.
    fn parse_strict(&self, value: &Value) -> Result<(), String>;
    /// The effective value as JSON: the lenient parse of a stored value, or
    /// the default when there is none.
    fn effective_json(&self, stored: Option<&Value>) -> Result<Value, String>;
    /// Makes a stored row (or the default, when there is none) live. A
    /// stored value that no longer parses uses the default. With
    /// `skip_unchanged`, a row whose `updated_at` and stored value are both
    /// the ones last applied is not parsed again.
    fn apply(&self, row: Option<&StoredSetting>, skip_unchanged: bool);
    fn current_any(&self) -> Box<dyn Any>;
}

/// What `apply` last did, and the version it published.
struct State {
    /// `None`: nothing applied yet. `Some(None)`: applied with no row.
    /// `Some(Some(t))`: applied the row last updated at `t`.
    applied: Option<Option<DateTime<Utc>>>,
    /// The stored value last applied. `updated_at` alone is not enough to
    /// skip a reload: two writes can share a timestamp.
    last_raw: Option<Value>,
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
                last_raw: None,
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

    fn parse_strict(&self, value: &Value) -> Result<(), String> {
        K::parse(value, true).map(|_| ())
    }

    fn effective_json(&self, stored: Option<&Value>) -> Result<Value, String> {
        let typed = match stored {
            Some(v) => K::parse(v, false)?,
            None => K::default_value(),
        };
        serde_json::to_value(&typed)
            .map_err(|e| format!("serializing the effective value failed: {e}"))
    }

    fn apply(&self, row: Option<&StoredSetting>, skip_unchanged: bool) {
        let at = row.map(|r| r.updated_at);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if skip_unchanged
            && state.applied == Some(at)
            && state.last_raw.as_ref() == row.map(|r| &r.value)
        {
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
        state.last_raw = row.map(|r| r.value.clone());
        let changed = typed != **self.current.load();
        if !changed && !first {
            return;
        }
        // The first apply publishes version 1; later effective changes bump it.
        if first {
            state.version = 1;
        } else {
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
