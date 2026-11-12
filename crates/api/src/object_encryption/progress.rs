use serde::{Deserialize, Serialize};
use services::files::{
    encryption::ObjectKeyRing,
    storage::{ObjectVerification, RewrapOutcome},
};
use std::collections::BTreeMap;

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Cursor {
    pub listing_attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<super::job_error::JobError>,
    pub continuation: Option<String>,
    pub last_key: Option<String>,
    pub complete: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending_keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<super::lease::Lease>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct ErrorSample {
    pub key: String,
    pub class: String,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Progress {
    pub processed: i64,
    pub total: i64,
    pub missing: i64,
    pub changed: i64,
    pub changed_keys: Vec<String>,
    pub errors: i64,
    pub error_samples: Vec<ErrorSample>,
    pub oversized: i64,
    pub oversized_keys: Vec<String>,
    pub unknown_key_ids: i64,
    pub unknown_key_ids_warned: bool,
    pub undecryptable_classes: BTreeMap<String, i64>,
    pub missing_keys: Vec<String>,
    pub by_key_id: BTreeMap<String, i64>,
    pub rewrapped: i64,
    pub already_current: i64,
    pub undecryptable: i64,
    pub undecryptable_keys: Vec<String>,
    pub stale: i64,
    pub verified: i64,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass: Option<bool>,
}

impl Progress {
    pub fn record_missing(&mut self, key: &str) {
        self.missing += 1;
        if self.missing_keys.len() < 100 {
            self.missing_keys.push(key.into());
        }
    }

    pub fn record_key_id(&mut self, key_id: Option<String>, ring: &ObjectKeyRing) {
        let bucket = match key_id {
            Some(id) if ring.lookup(&id).is_some() => id,
            Some(_) => "unknown".into(),
            None => "none".into(),
        };
        *self.by_key_id.entry(bucket).or_default() += 1;
    }

    pub fn retain_ring_keys(&mut self, ring: &ObjectKeyRing) {
        // Normalize progress written before unknown metadata was bucketed, or
        // resumed after an operator removed an old read key from the ring.
        self.missing_keys.truncate(100);
        self.changed_keys.truncate(100);
        self.error_samples.truncate(100);
        self.oversized_keys.truncate(100);
        self.undecryptable_keys.truncate(100);
        let counts = std::mem::take(&mut self.by_key_id);
        for (id, count) in counts {
            let bucket = if id == "none" || ring.lookup(&id).is_some() {
                id
            } else {
                "unknown".into()
            };
            *self.by_key_id.entry(bucket).or_default() += count;
        }
    }

    pub fn record_rewrap(&mut self, key: &str, outcome: RewrapOutcome) {
        match outcome {
            RewrapOutcome::AlreadyCurrent => self.already_current += 1,
            RewrapOutcome::Rewrapped { .. } | RewrapOutcome::RewrappedUnknownKeyId => {
                self.rewrapped += 1
            }
            RewrapOutcome::UndecryptableUnknownKeyId => {
                self.record_undecryptable(key, "unknown_key_id")
            }
            RewrapOutcome::Undecryptable => self.record_undecryptable(key, "decryption_failed"),
        }
    }

    pub fn record_verification(&mut self, key: &str, result: ObjectVerification) {
        if !result.is_current {
            self.stale += 1;
        }
        if result.decrypts_under_write_key {
            self.verified += 1;
        } else if !result.decrypts_under_previous_key {
            self.record_undecryptable(
                key,
                if result.unknown_key_id {
                    "unknown_key_id"
                } else {
                    "decryption_failed"
                },
            );
        }
    }

    pub fn record_undecryptable(&mut self, key: &str, class: &str) {
        self.undecryptable += 1;
        *self.undecryptable_classes.entry(class.into()).or_default() += 1;
        if self.undecryptable_keys.len() < 100 {
            self.undecryptable_keys.push(key.into());
        }
    }
}

impl Progress {
    pub fn record_changed(&mut self, key: &str) {
        self.changed += 1;
        if self.changed_keys.len() < 100 {
            self.changed_keys.push(key.into());
        }
    }

    pub fn record_error(&mut self, key: &str, class: &str) {
        self.errors += 1;
        if self.error_samples.len() < 100 {
            self.error_samples.push(ErrorSample {
                key: key.into(),
                class: class.into(),
            });
        }
    }

    pub fn record_oversized(&mut self, key: &str, stale: bool) {
        self.oversized += 1;
        if stale {
            self.stale += 1;
        }
        if self.oversized_keys.len() < 100 {
            self.oversized_keys.push(key.into());
        }
    }

    pub const fn verification_passes(&self) -> bool {
        self.complete
            && self.stale == 0
            && self.undecryptable == 0
            && self.errors == 0
            && self.oversized == 0
    }
}

#[cfg(test)]
#[path = "progress_tests.rs"]
mod tests;
