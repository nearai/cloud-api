use serde::{Deserialize, Serialize};
use services::files::{
    encryption::ObjectKeyRing,
    storage::{ObjectVerification, RewrapOutcome},
};
use std::collections::BTreeMap;

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Cursor {
    pub continuation: Option<String>,
    pub last_key: Option<String>,
    pub complete: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Progress {
    pub processed: i64,
    pub total: i64,
    pub missing: i64,
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
            RewrapOutcome::Rewrapped { .. } => self.rewrapped += 1,
            RewrapOutcome::Undecryptable => self.record_undecryptable(key),
        }
    }

    pub fn record_verification(&mut self, key: &str, result: ObjectVerification) {
        if !result.is_current {
            self.stale += 1;
        }
        if result.is_decryptable {
            self.verified += 1;
        } else {
            self.record_undecryptable(key);
        }
    }

    fn record_undecryptable(&mut self, key: &str) {
        self.undecryptable += 1;
        if self.undecryptable_keys.len() < 100 {
            self.undecryptable_keys.push(key.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_counts_stale_metadata_and_invalid_ciphertext_independently() {
        // Given a fresh progress tally.
        let mut progress = Progress::default();
        // When an object has stale metadata and cannot decrypt with the write key.
        progress.record_verification(
            "workspace/file",
            ObjectVerification {
                is_current: false,
                is_decryptable: false,
            },
        );
        // Then both conditions prevent a passing verification.
        assert_eq!(
            (progress.stale, progress.undecryptable, progress.verified),
            (1, 1, 0)
        );
    }

    #[test]
    fn undecryptable_samples_are_bounded_and_survive_resumption() {
        // Given more invalid objects than the sample limit.
        let mut progress = Progress::default();
        for number in 0..110 {
            progress.record_rewrap(&format!("workspace/{number}"), RewrapOutcome::Undecryptable);
        }
        // When restoring persisted progress.
        let restored: Progress =
            serde_json::from_value(serde_json::to_value(progress).unwrap()).unwrap();
        // Then every failure is counted and only the bounded sample remains.
        assert_eq!(
            (restored.undecryptable, restored.undecryptable_keys.len()),
            (110, 100)
        );
    }

    #[test]
    fn cursor_retains_the_next_page_and_last_completed_key() {
        // Given a committed object page.
        let cursor = Cursor {
            continuation: Some("next-token".into()),
            last_key: Some("workspace/file".into()),
            complete: false,
        };
        // When restoring its persisted cursor.
        let restored: Cursor =
            serde_json::from_value(serde_json::to_value(cursor).unwrap()).unwrap();
        // Then resumption starts with the next token and preserves the completed boundary.
        assert_eq!(
            (
                restored.continuation.as_deref(),
                restored.last_key.as_deref(),
                restored.complete
            ),
            (Some("next-token"), Some("workspace/file"), false)
        );
    }
    #[test]
    fn scan_groups_legacy_and_known_object_metadata() {
        // Given a fresh inventory tally.
        let mut progress = Progress::default();
        let ring = ObjectKeyRing::new(
            "current".into(),
            [9; 32],
            vec![("previous".into(), [7; 32])],
        )
        .unwrap();
        // When legacy and named metadata are counted.
        progress.record_key_id(None, &ring);
        progress.record_key_id(Some("previous".into()), &ring);
        progress.record_key_id(Some("current".into()), &ring);
        progress.record_key_id(None, &ring);
        // Then each metadata identity has its own count.
        assert_eq!(
            progress.by_key_id,
            BTreeMap::from([
                ("none".into(), 2),
                ("previous".into(), 1),
                ("current".into(), 1)
            ])
        );
    }

    #[test]
    fn rewrap_counts_current_and_updated_objects_separately() {
        // Given a fresh rewrap tally.
        let mut progress = Progress::default();
        // When all rewrap outcomes are recorded.
        for outcome in [
            RewrapOutcome::AlreadyCurrent,
            RewrapOutcome::Rewrapped {
                from: "previous".into(),
            },
            RewrapOutcome::Undecryptable,
        ] {
            progress.record_rewrap("workspace/file", outcome);
        }
        // Then each outcome is counted separately.
        assert_eq!(
            (
                progress.already_current,
                progress.rewrapped,
                progress.undecryptable
            ),
            (1, 1, 1)
        );
    }
}

#[cfg(test)]
mod resumption_tests {
    use super::*;

    #[test]
    fn resuming_counts_folds_unconfigured_metadata_into_unknown() {
        // Given persisted counts from an older ring or worker.
        let ring = ObjectKeyRing::new("current".into(), [9; 32], vec![]).unwrap();
        let mut progress: Progress = serde_json::from_value(serde_json::json!({
            "by_key_id":{"current":2,"none":3,"retired":4,"arbitrary":5,"unknown":1}
        }))
        .unwrap();
        // When the worker resumes using the current ring.
        progress.retain_ring_keys(&ring);
        // Then only configured IDs and fixed inventory buckets survive.
        assert_eq!(
            progress.by_key_id,
            BTreeMap::from([
                ("current".into(), 2),
                ("none".into(), 3),
                ("unknown".into(), 10)
            ])
        );
    }
}
