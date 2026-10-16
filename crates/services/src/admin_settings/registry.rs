//! The setting-key registry. A [`SettingKey`] is a strategy owned by the
//! consumer of a setting: it names the key, lists its fields, parses its
//! value, and builds the handle value its owner reads. The admin settings
//! service holds registered keys type-erased and knows no key's type.

use std::any::Any;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde_json::Value;

use super::{AdminSettingsRepository, AdminSettingsService};

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
    /// Build the published form. `version` increments on every effective change.
    fn publish(value: &Self::Value, version: u64) -> Self::Published;
}

/// A key with its type erased, so the service can hold keys of any type.
pub(super) trait ErasedKey: Send + Sync {
    fn name(&self) -> &'static str;
    fn fields(&self) -> &'static [&'static str];
    /// Strict parse of PATCH input.
    fn parse_strict(&self, value: &Value) -> Result<(), String>;
    /// The effective value as JSON: the lenient parse of a stored value, or
    /// the default when there is none.
    fn effective_json(&self, stored: Option<&Value>) -> Result<Value, String>;
    /// Makes a stored value (or the default, when there is none) live. A
    /// stored value that no longer parses uses the default.
    fn apply(&self, stored: Option<&Value>);
    fn current_any(&self) -> Box<dyn Any>;
}

struct Registered<K: SettingKey> {
    handle: Arc<ArcSwap<K::Published>>,
    current: ArcSwap<K::Value>,
    version: u64,
}

impl<K: SettingKey> Registered<K> {
    fn new(handle: Arc<ArcSwap<K::Published>>) -> Self {
        Self {
            handle,
            current: ArcSwap::from_pointee(K::default_value()),
            version: 0,
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

    fn parse_strict(&self, value: &Value) -> Result<(), String> {
        K::parse(value, true).map(|_| ())
    }

    fn effective_json(&self, stored: Option<&Value>) -> Result<Value, String> {
        let typed = match stored {
            Some(v) => K::parse(v, false)?,
            None => K::default_value(),
        };
        Ok(serde_json::to_value(&typed).unwrap_or(Value::Null))
    }

    fn apply(&self, stored: Option<&Value>) {
        let typed = match stored.map(|v| K::parse(v, false)) {
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
        self.handle
            .store(Arc::new(K::publish(&typed, self.version)));
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
