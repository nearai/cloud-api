use super::{bad, decrypt_ring_envelope, envelope, Field, KeyRing, Mode, MARKER};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Classification {
    Plaintext,
    Current,
    Stale(String),
    Invalid,
}

pub(super) fn classify(ring: &KeyRing, field: &Field, id: Uuid, raw: &str) -> Classification {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Classification::Plaintext;
    };
    if value[MARKER] != true {
        return Classification::Plaintext;
    }
    if database::field_encryption::decrypt_with_ring(&value, ring, field.table, field.column, id)
        .is_err()
    {
        return Classification::Invalid;
    }
    match database::field_encryption::envelope_key_id(&value) {
        Some(key_id) if key_id == ring.write_id() => Classification::Current,
        Some(key_id) => Classification::Stale(key_id.to_string()),
        None => Classification::Invalid,
    }
}

pub(super) fn rewrap_envelope(
    ring: &KeyRing,
    field: &Field,
    id: Uuid,
    raw: &str,
) -> anyhow::Result<String> {
    let plaintext = decrypt_ring_envelope(ring, field, id, raw)?;
    envelope(ring.write_key(), ring.write_id(), field, id, &plaintext).context("rewrap failed")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Encrypt,
    Rewrap,
    Verify,
}

pub(super) fn job_action(
    mode: Mode,
    actions: &[String],
) -> Result<Action, (axum::http::StatusCode, axum::Json<Value>)> {
    match (mode, actions) {
        (Mode::Verify, [action]) if action == "verify" => Ok(Action::Verify),
        (Mode::DryRun | Mode::Execute, [action]) if action == "encrypt" => Ok(Action::Encrypt),
        (Mode::DryRun | Mode::Execute, [action]) if action == "rewrap" => Ok(Action::Rewrap),
        _ => Err(bad("actions do not match the requested job mode")),
    }
}

pub(super) fn validate_delay(
    delay: Option<i64>,
) -> Result<(), (axum::http::StatusCode, axum::Json<Value>)> {
    if delay.is_some_and(|value| !(0..=10_000).contains(&value)) {
        return Err(bad("batch_delay_ms must be between 0 and 10000"));
    }
    Ok(())
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct KeyCounts {
    pub totals: BTreeMap<String, i64>,
    pub fields: BTreeMap<String, BTreeMap<String, i64>>,
}

impl KeyCounts {
    pub fn record(&mut self, field: &Field, raw: &str) {
        if let Ok(value) = serde_json::from_str::<Value>(raw) {
            if let Some(key_id) = database::field_encryption::envelope_key_id(&value) {
                *self.totals.entry(key_id.to_string()).or_default() += 1;
                *self
                    .fields
                    .entry(format!("{}.{}", field.table, field.column))
                    .or_default()
                    .entry(key_id.to_string())
                    .or_default() += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Scope, FIELDS};
    use super::*;
    use serde_json::json;

    #[test]
    fn classification_checks_key_identity_authentication_and_context() {
        let ring = KeyRing::new("db-v2".into(), [9; 32], vec![("db-v1".into(), [7; 32])]).unwrap();
        let field = &FIELDS[0];
        let id = Uuid::new_v4();
        assert_eq!(
            classify(&ring, field, id, "plaintext"),
            Classification::Plaintext
        );
        let current = envelope(&[9; 32], "db-v2", field, id, "current").unwrap();
        assert_eq!(
            classify(&ring, field, id, &current),
            Classification::Current
        );
        let old = envelope(&[7; 32], "db-v1", field, id, "previous").unwrap();
        assert_eq!(
            classify(&ring, field, id, &old),
            Classification::Stale("db-v1".into())
        );
        assert_eq!(
            classify(&ring, field, Uuid::new_v4(), &old),
            Classification::Invalid
        );
        assert_eq!(
            classify(&ring, &FIELDS[1], id, &old),
            Classification::Invalid
        );
        let unknown = envelope(&[8; 32], "db-v3", field, id, "unknown").unwrap();
        assert_eq!(
            classify(&ring, field, id, &unknown),
            Classification::Invalid
        );
        let wrong_key = envelope(&[8; 32], "db-v2", field, id, "wrong").unwrap();
        assert_eq!(
            classify(&ring, field, id, &wrong_key),
            Classification::Invalid
        );
        assert_eq!(
            classify(&ring, field, id, &json!({MARKER:true}).to_string()),
            Classification::Invalid
        );
        assert_eq!(
            classify(&ring, field, id, &json!({MARKER:false}).to_string()),
            Classification::Plaintext
        );
        let rewritten = rewrap_envelope(&ring, field, id, &old).unwrap();
        let reduced = KeyRing::new("db-v2".into(), [9; 32], vec![]).unwrap();
        assert_eq!(
            classify(&reduced, field, id, &rewritten),
            Classification::Current
        );
        assert_eq!(
            decrypt_ring_envelope(&reduced, field, id, &rewritten).unwrap(),
            "previous"
        );
        let mut counts = KeyCounts::default();
        counts.record(field, &old);
        counts.record(field, &rewritten);
        let restored: KeyCounts =
            serde_json::from_value(serde_json::to_value(counts).unwrap()).unwrap();
        assert_eq!(restored.totals["db-v1"], 1);
        assert_eq!(restored.fields["response_items.item"]["db-v2"], 1);
    }

    #[test]
    fn action_mode_matrix_is_exact() {
        for mode in [Mode::DryRun, Mode::Execute, Mode::Verify] {
            for action in ["encrypt", "rewrap", "verify", "unknown"] {
                let expected = match mode {
                    Mode::Verify => action == "verify",
                    Mode::DryRun | Mode::Execute => matches!(action, "encrypt" | "rewrap"),
                };
                assert_eq!(job_action(mode, &[action.into()]).is_ok(), expected);
                assert!(job_action(mode, &[action.into(), action.into()]).is_err());
            }
            assert!(job_action(mode, &[]).is_err());
        }
    }

    #[test]
    fn rewrap_errors_have_a_distinct_safe_class() {
        assert_eq!(
            super::super::classify_job_error(&anyhow::anyhow!("rewrap failed")),
            "rewrap_failed"
        );
    }

    #[test]
    fn delay_bounds_and_scope_options_round_trip() {
        for delay in [None, Some(0), Some(1), Some(10_000)] {
            assert!(validate_delay(delay).is_ok());
        }
        for delay in [-1, 10_001, i64::MAX] {
            let (status, body) = validate_delay(Some(delay)).unwrap_err();
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
            assert_eq!(
                body["error"]["message"],
                "batch_delay_ms must be between 0 and 10000"
            );
        }
        let mut scope: Scope = serde_json::from_value(json!({"fields":[{"table":"files","column":"filename"}], "options":{"batch_delay_ms":5}})).unwrap();
        super::super::normalize_scope(&mut scope);
        assert_eq!(super::super::selected(&scope).unwrap().len(), 1);
        let restored: Scope = serde_json::from_value(serde_json::to_value(scope).unwrap()).unwrap();
        assert_eq!(restored.options.batch_delay_ms, Some(5));
    }
}
